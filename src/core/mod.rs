// Copyright 2022 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Mixnet core logic. This module tries to be network agnostic.

// Get a bunch of these from [mut_]array_refs
#![allow(clippy::ptr_offset_with_cast)]

mod config;
mod cover;
mod fragment;
mod kx_store;
mod packet_queues;
mod replay_filter;
mod request_builder;
mod sessions;
mod sphinx;
mod surb_keystore;
mod topology;
mod util;

pub use self::{
	config::Config,
	fragment::{MessageId, MESSAGE_ID_SIZE},
	kx_store::KxPublicStore,
	packet_queues::AddressedPacket,
	sessions::{RelSessionIndex, SessionIndex, SessionPhase, SessionStatus},
	sphinx::{
		KxPublic, MixnodeIndex, Packet, PeerId, RawMixnodeIndex, Surb, KX_PUBLIC_SIZE,
		MAX_MIXNODE_INDEX, PACKET_SIZE, PEER_ID_SIZE, SURB_SIZE,
	},
	topology::{Mixnode, NetworkStatus, TopologyErr},
};
use self::{
	cover::{gen_cover_packet, CoverKind},
	fragment::{fragment_blueprints, FragmentAssembler},
	kx_store::KxStore,
	packet_queues::{AuthoredPacketQueue, ForwardPacket, ForwardPacketQueue},
	replay_filter::ReplayFilter,
	request_builder::RequestBuilder,
	sessions::{Session, SessionSlot, Sessions},
	sphinx::{
		complete_reply_packet, decrypt_reply_payload, kx_public, mut_payload_data, peel, Action,
		Delay, PeelErr, PAYLOAD_DATA_SIZE, PAYLOAD_SIZE,
	},
	surb_keystore::SurbKeystore,
	topology::Topology,
	util::default_boxed_array,
};
use arrayref::{array_mut_ref, array_ref};
use arrayvec::ArrayVec;
use bitflags::bitflags;
use either::Either;
use log::{error, warn};
use multiaddr::Multiaddr;
use rand::{Rng, RngCore};
use std::{
	cmp::{max, min},
	collections::HashSet,
	sync::Arc,
	time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub struct MixnodeId {
	pub session_index: SessionIndex,
	/// Index of the mixnode in the list for the session with index `session_index`.
	pub mixnode_index: MixnodeIndex,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Message {
	Request { session_index: SessionIndex, data: Vec<u8>, surbs: Vec<Surb> },
	Reply { id: MessageId, data: Vec<u8> },
}

#[derive(Debug, thiserror::Error)]
pub enum PostErr {
	#[error("Message would need to be split into too many fragments")]
	TooManyFragments,
	#[error("Bad session index: {0}")]
	BadSessionIndex(SessionIndex),
	#[error("Requests and replies currently blocked for session {0}")]
	RequestsAndRepliesBlocked(SessionIndex),
	#[error("Mixnodes not yet known for session {0}")]
	SessionEmpty(SessionIndex),
	#[error("Mixnet disabled for session {0}")]
	SessionDisabled(SessionIndex),
	#[error("There is not enough space in the authored packet queue")]
	NotEnoughSpaceInQueue,
	#[error("Topology error: {0}")]
	Topology(TopologyErr),
	#[error("Bad SURB")]
	BadSurb,
}

fn post_session(
	sessions: &mut Sessions,
	status: SessionStatus,
	index: SessionIndex,
) -> Result<&mut Session, PostErr> {
	let rel_index = match status.current_index.wrapping_sub(index) {
		0 => RelSessionIndex::Current,
		1 => RelSessionIndex::Prev,
		_ => return Err(PostErr::BadSessionIndex(index)),
	};
	if !status.phase.allow_requests_and_replies(rel_index) {
		return Err(PostErr::RequestsAndRepliesBlocked(index))
	}
	match &mut sessions[rel_index] {
		SessionSlot::Empty => Err(PostErr::SessionEmpty(index)),
		SessionSlot::Disabled => Err(PostErr::SessionDisabled(index)),
		SessionSlot::Full(session) => Ok(session),
	}
}

bitflags! {
	/// Flags to indicate which previously queried things are now invalid.
	pub struct Invalidated: u32 {
		/// The reserved peers returned by `reserved_peer_addresses()`.
		const RESERVED_PEERS = 0b001;
		/// The deadline returned by `next_forward_packet_deadline()`.
		const NEXT_FORWARD_PACKET_DEADLINE = 0b010;
		/// The effective deadline returned by `next_authored_packet_delay()`. The delay (and thus
		/// the effective deadline) is randomly generated according to an exponential distribution
		/// each time the function is called, but the last returned deadline remains valid until
		/// this bit indicates otherwise. Due to the memoryless nature of exponential
		/// distributions, it is harmless for this bit to be set spuriously.
		const NEXT_AUTHORED_PACKET_DEADLINE = 0b100;
	}
}

pub struct Mixnet {
	config: Config,

	/// Index and phase of current session.
	session_status: SessionStatus,
	/// Keystore for the per-session key-exchange keys.
	kx_store: KxStore,
	/// Current and previous sessions.
	sessions: Sessions,

	/// Queue of packets to be forwarded, after some delay.
	forward_packet_queue: ForwardPacketQueue,

	/// Keystore for SURB payload encryption keys.
	surb_keystore: SurbKeystore,
	/// Reassembles fragments into messages. Note that for simplicity there is just one assembler
	/// for everything (requests and replies across all sessions).
	fragment_assembler: FragmentAssembler,

	/// Flags to indicate which previously queried things are now invalid.
	invalidated: Invalidated,
}

impl Mixnet {
	pub fn new(config: Config, kx_public_store: Arc<KxPublicStore>) -> Self {
		let forward_packet_queue = ForwardPacketQueue::new(config.forward_packet_queue_capacity);

		let surb_keystore = SurbKeystore::new(config.surb_keystore_capacity);
		let fragment_assembler = FragmentAssembler::new(
			config.max_incomplete_messages,
			config.max_incomplete_fragments,
			config.max_fragments_per_message,
		);

		Self {
			config,

			session_status: SessionStatus {
				current_index: 0,
				phase: SessionPhase::ConnectToCurrent, // Doesn't really matter
			},
			sessions: Default::default(),
			kx_store: KxStore::new(kx_public_store),

			forward_packet_queue,

			surb_keystore,
			fragment_assembler,

			invalidated: Invalidated::empty(),
		}
	}

	/// Sets the current session index and phase. The current and previous mixnodes may need to be
	/// provided after calling this; see `maybe_set_mixnodes`.
	pub fn set_session_status(&mut self, session_status: SessionStatus) {
		if self.session_status == session_status {
			return
		}

		// Shift sessions in self.sessions when current session changes
		if self.session_status.current_index != session_status.current_index {
			if session_status.current_index.saturating_sub(self.session_status.current_index) == 1 {
				self.sessions.advance_by_one();
			} else {
				if self.sessions.current.is_full() {
					warn!(
						target: self.config.log_target,
						"Unexpected session index {}; previous session index was {}",
						session_status.current_index,
						self.session_status.current_index
					);
				}
				self.sessions = Default::default();
			}
		}

		// Discard sessions which are no longer needed
		if !session_status.phase.need_prev() {
			self.sessions.prev = SessionSlot::Disabled;
		}
		let min_needed_rel_session_index = if session_status.phase.need_prev() {
			RelSessionIndex::Prev
		} else {
			RelSessionIndex::Current
		};
		self.kx_store
			.discard_sessions_before(min_needed_rel_session_index + session_status.current_index);

		// For simplicity just mark these as invalid whenever anything changes. This should happen
		// at most once a minute or so.
		self.invalidated |=
			Invalidated::RESERVED_PEERS | Invalidated::NEXT_AUTHORED_PACKET_DEADLINE;

		self.session_status = session_status;
	}

	/// Sets the mixnodes for the specified session, if they are needed.
	pub fn maybe_set_mixnodes<E>(
		&mut self,
		rel_session_index: RelSessionIndex,
		mixnodes: impl FnOnce() -> Result<Vec<Mixnode>, E>,
	) -> Result<(), E> {
		// Create the Session only if the slot is empty. If the slot is disabled, don't even try.
		let session = &mut self.sessions[rel_session_index];
		if !session.is_empty() {
			return Ok(())
		}

		let mut rng = rand::thread_rng();

		// Build Topology struct
		let session_index = rel_session_index + self.session_status.current_index;
		let mut mixnodes = mixnodes()?;
		if mixnodes.len() < self.config.min_mixnodes {
			error!(
				target: self.config.log_target,
				"Insufficient mixnodes registered for session {session_index} \
				({} mixnodes registered, need {}); \
				mixnet will not be available during this session",
				mixnodes.len(),
				self.config.min_mixnodes
			);
			*session = SessionSlot::Disabled;
			return Ok(())
		}
		let max_mixnodes = (MAX_MIXNODE_INDEX + 1) as usize;
		if mixnodes.len() > max_mixnodes {
			warn!(
				target: self.config.log_target,
				"Too many mixnodes ({}, max {max_mixnodes}); ignoring excess",
				mixnodes.len()
			);
			mixnodes.truncate(max_mixnodes);
		}
		let Some(local_kx_public) = self.kx_store.public().public_for_session(session_index) else {
			error!(target: self.config.log_target,
				"Key-exchange keys for session {session_index} discarded already; \
				mixnet will not be available");
			*session = SessionSlot::Disabled;
			return Ok(())
		};
		let topology =
			Topology::new(&mut rng, mixnodes, &local_kx_public, self.config.num_gateway_mixnodes);

		// Determine session config
		let config = if topology.is_mixnode() {
			&self.config.mixnode_session
		} else {
			match &self.config.non_mixnode_session {
				Some(config) => config,
				None => {
					*session = SessionSlot::Disabled;
					return Ok(())
				},
			}
		};

		// Build Session struct
		*session = SessionSlot::Full(Session {
			topology,
			authored_packet_queue: AuthoredPacketQueue::new(config.authored_packet_queue_capacity),
			mean_authored_packet_period: config.mean_authored_packet_period,
			replay_filter: ReplayFilter::new(&mut rng),
		});

		self.invalidated |=
			Invalidated::RESERVED_PEERS | Invalidated::NEXT_AUTHORED_PACKET_DEADLINE;

		Ok(())
	}

	pub fn reserved_peer_addresses(&self) -> HashSet<Multiaddr> {
		self.sessions
			.iter()
			.flat_map(|session| session.topology.reserved_peer_addresses())
			.cloned()
			.collect()
	}

	pub fn handle_packet(&mut self, packet: &Packet) -> Option<Message> {
		self.kx_store.add_pending_session_secrets();

		let mut out = [0; PACKET_SIZE];
		let res = self.sessions.enumerate_mut().find_map(|(rel_session_index, session)| {
			if session.replay_filter.contains(kx_public(packet)) {
				return Some(Err(Either::Left(
					"Packet key-exchange public key found in replay filter",
				)))
			}

			let session_index = rel_session_index + self.session_status.current_index;
			// If secret key for session not found, try other session
			let kx_shared_secret =
				self.kx_store.session_exchange(session_index, kx_public(packet))?;

			match peel(&mut out, packet, &kx_shared_secret) {
				// Bad MAC possibly means we used the wrong secret; try other session
				Err(PeelErr::Mac) => None,
				// Any other error means the packet is bad; just discard it
				Err(err) => Some(Err(Either::Right(err))),
				Ok(action) => Some(Ok((action, session_index, session))),
			}
		});

		let (action, session_index, session) = match res {
			None => {
				error!(
					target: self.config.log_target,
					"Failed to peel packet; either bad MAC or unknown secret"
				);
				return None
			},
			Some(Err(err)) => {
				error!(target: self.config.log_target, "Failed to peel packet: {err}");
				return None
			},
			Some(Ok(x)) => x,
		};

		match action {
			Action::ForwardTo { target, delay } => {
				if !session.topology.is_mixnode() {
					error!(target: self.config.log_target,
						"Received packet to forward despite not being a mixnode in the session; discarding");
					return None
				}

				if self.forward_packet_queue.remaining_capacity() == 0 {
					warn!(target: self.config.log_target, "Dropped forward packet; forward queue full");
					return None
				}

				// After the is_mixnode check to avoid inserting anything into the replay filters
				// for sessions where we are not a mixnode
				session.replay_filter.insert(kx_public(packet));

				match session.topology.target_to_peer_id(&target) {
					Ok(peer_id) => {
						let deadline =
							Instant::now() + delay.to_duration(self.config.mean_forwarding_delay);
						let forward_packet = ForwardPacket {
							deadline,
							packet: AddressedPacket { peer_id, packet: out.into() },
						};
						if self.forward_packet_queue.insert(forward_packet) {
							self.invalidated |= Invalidated::NEXT_FORWARD_PACKET_DEADLINE;
						}
					},
					Err(err) => error!(
						target: self.config.log_target,
						"Failed to map target {target:?} to peer ID: {err}"
					),
				}

				None
			},
			Action::DeliverRequest => {
				let payload_data = array_ref![out, 0, PAYLOAD_DATA_SIZE];

				if !session.topology.is_mixnode() {
					error!(target: self.config.log_target,
						"Received request packet despite not being a mixnode in the session; discarding");
					return None
				}

				// After the is_mixnode check to avoid inserting anything into the replay filters
				// for sessions where we are not a mixnode
				session.replay_filter.insert(kx_public(packet));

				// Add to fragment assembler and return any completed message
				self.fragment_assembler.insert(payload_data, self.config.log_target).map(
					|message| Message::Request {
						session_index,
						data: message.data,
						surbs: message.surbs,
					},
				)
			},
			Action::DeliverReply { surb_id } => {
				let payload = array_mut_ref![out, 0, PAYLOAD_SIZE];

				// Note that we do not insert anything into the replay filter here. The SURB ID
				// lookup will fail for replayed SURBs, so explicit replay prevention is not
				// necessary. The main reason for avoiding the replay filter here is so that it
				// does not need to be allocated at all for sessions where we are not a mixnode.

				// Lookup payload encryption keys and decrypt payload
				let Some(entry) = self.surb_keystore.entry(&surb_id) else {
					error!(target: self.config.log_target,
						"Received reply with unrecognised SURB ID {surb_id:x?}; discarding");
					return None
				};
				let res = decrypt_reply_payload(payload, entry.keys());
				entry.remove();
				if let Err(err) = res {
					error!(target: self.config.log_target, "Failed to decrypt reply payload: {err}");
					return None
				}
				let payload_data = array_ref![payload, 0, PAYLOAD_DATA_SIZE];

				// Add to fragment assembler and return any completed message
				self.fragment_assembler.insert(payload_data, self.config.log_target).map(
					|message| {
						if !message.surbs.is_empty() {
							warn!(target: self.config.log_target,
								"Reply message included SURBs; discarding them");
						}
						Message::Reply { id: message.id, data: message.data }
					},
				)
			},
			Action::DeliverCover { cover_id: _ } => None,
		}
	}

	pub fn next_forward_packet_deadline(&self) -> Option<Instant> {
		self.forward_packet_queue.next_deadline()
	}

	/// Pop and return the packet at the head of the forward packet queue. Returns `None` if the
	/// queue is empty.
	pub fn pop_next_forward_packet(&mut self) -> Option<AddressedPacket> {
		self.invalidated |= Invalidated::NEXT_FORWARD_PACKET_DEADLINE;
		self.forward_packet_queue.pop().map(|packet| packet.packet)
	}

	pub fn next_authored_packet_delay(&self) -> Option<Duration> {
		// Send packets at the maximum rate of any active session; pop_next_authored_packet will
		// choose between the sessions randomly based on their rates
		self.sessions
			.enumerate()
			.filter(|(rel_session_index, _)| {
				self.session_status.phase.gen_cover_packets(*rel_session_index)
			})
			.map(|(_, session)| session.mean_authored_packet_period)
			.min()
			.map(|mean| {
				let delay: f64 = rand::thread_rng().sample(rand_distr::Exp1);
				// Cap at 10x the mean; this is about the 99.995th percentile. This avoids
				// potential panics in mul_f64() due to overflow.
				mean.mul_f64(delay.min(10.0))
			})
	}

	/// Either generate and return a cover packet or pop and return the packet at the head of one
	/// of the authored packet queues. May return `None` if cover packets are disabled, we fail to
	/// generate a cover packet, or there are no active sessions (though in the no active sessions
	/// case `next_authored_packet_delay` should return `None` and so this function should not
	/// really be called).
	pub fn pop_next_authored_packet(&mut self, ns: &dyn NetworkStatus) -> Option<AddressedPacket> {
		// This function should be called according to a Poisson process. Randomly choosing between
		// sessions and cover kinds here is equivalent to there being multiple independent Poisson
		// processes; see https://www.randomservices.org/random/poisson/Splitting.html
		let mut rng = rand::thread_rng();

		// First pick the session
		let sessions: ArrayVec<_, 2> = self
			.sessions
			.enumerate_mut()
			.filter(|(rel_session_index, _)| {
				self.session_status.phase.gen_cover_packets(*rel_session_index)
			})
			.collect();
		let (rel_session_index, session) = match sessions.into_inner() {
			Ok(sessions) => {
				// Both sessions active. We choose randomly based on their rates.
				let periods = sessions
					// TODO This could be replaced with .each_ref() once it is stabilised, allowing
					// the collect/into_inner/expect at the end to be dropped
					.iter()
					.map(|(_, session)| session.mean_authored_packet_period.as_secs_f64())
					.collect::<ArrayVec<_, 2>>()
					.into_inner()
					.expect("Input is array of length 2");
				let [session_0, session_1] = sessions;
				// Rate is 1/period, and (1/a)/((1/a)+(1/b)) = b/(a+b)
				if rng.gen_bool(periods[1] / (periods[0] + periods[1])) {
					session_0
				} else {
					session_1
				}
			},
			// Either just one active session or no active sessions. This function shouldn't really
			// be called in the latter case, as next_authored_packet_delay() should return None.
			Err(mut sessions) => sessions.pop()?,
		};

		self.invalidated |= Invalidated::NEXT_AUTHORED_PACKET_DEADLINE;

		// Choose randomly between drop and loop cover packet
		if rng.gen_bool(self.config.loop_cover_proportion) {
			gen_cover_packet(&mut rng, &session.topology, ns, CoverKind::Loop, &self.config)
		} else {
			self.session_status
				.phase
				.allow_requests_and_replies(rel_session_index)
				.then(|| session.authored_packet_queue.pop())
				.flatten()
				.or_else(|| {
					gen_cover_packet(&mut rng, &session.topology, ns, CoverKind::Drop, &self.config)
				})
		}
	}

	/// Post a request message. If `destination` is `None`, a destination mixnode is chosen at
	/// random and (on success) the session and mixnode indices are written back to `destination`.
	/// The message is split into fragments and each fragment is sent over a different path to the
	/// destination. Returns the maximum total forwarding delay for any fragment/SURB pair; this
	/// should give a lower bound on the time it takes for a reply to arrive (it is possible for a
	/// reply to arrive sooner if a mixnode misbehaves).
	pub fn post_request(
		&mut self,
		destination: &mut Option<MixnodeId>,
		data: &[u8],
		num_surbs: usize,
		ns: &dyn NetworkStatus,
	) -> Result<Duration, PostErr> {
		let mut rng = rand::thread_rng();

		// Split the message into fragments
		let mut message_id = [0; MESSAGE_ID_SIZE];
		rng.fill_bytes(&mut message_id);
		let fragment_blueprints = match fragment_blueprints(&message_id, data, num_surbs) {
			Some(fragment_blueprints)
				if fragment_blueprints.len() <= self.config.max_fragments_per_message =>
				fragment_blueprints,
			_ => return Err(PostErr::TooManyFragments),
		};

		// Grab the session and check there's room in the queue
		let session_index = destination.map_or_else(
			|| {
				self.session_status.phase.default_request_session() +
					self.session_status.current_index
			},
			|destination| destination.session_index,
		);
		let session = post_session(&mut self.sessions, self.session_status, session_index)?;
		// TODO Something better than this
		if fragment_blueprints.len() > session.authored_packet_queue.remaining_capacity() {
			return Err(PostErr::NotEnoughSpaceInQueue)
		}

		// Generate the packets and push them into the queue
		let request_builder = RequestBuilder::new(
			&mut rng,
			&session.topology,
			ns,
			destination.map(|destination| destination.mixnode_index),
		)
		.map_err(PostErr::Topology)?;
		let mut max_request_delay = Delay::zero();
		let mut max_reply_delay = Delay::zero();
		for fragment_blueprint in fragment_blueprints {
			let (packet, delay) = request_builder
				.build_packet(
					&mut rng,
					|fragment, rng| {
						fragment_blueprint.write_except_surbs(fragment);
						for surb in fragment_blueprint.surbs(fragment) {
							// TODO Currently we don't clean up keystore entries on failure
							let (id, keys) = self.surb_keystore.insert(rng, self.config.log_target);
							let num_hops = self.config.num_hops;
							let delay =
								request_builder.build_surb(surb, keys, rng, &id, num_hops)?;
							max_reply_delay = max(max_reply_delay, delay);
						}
						Ok(())
					},
					self.config.num_hops,
				)
				.map_err(PostErr::Topology)?;
			session.authored_packet_queue.push(packet);
			max_request_delay = max(max_request_delay, delay);
		}

		*destination =
			Some(MixnodeId { session_index, mixnode_index: request_builder.destination_index() });
		let max_delay = max_request_delay + max_reply_delay;
		Ok(max_delay.to_duration(self.config.mean_forwarding_delay))
	}

	/// Post a reply message using SURBs. The session index must match the session the SURBs were
	/// generated for. SURBs are removed from `surbs` on use.
	pub fn post_reply(
		&mut self,
		surbs: &mut Vec<Surb>,
		session_index: SessionIndex,
		message_id: &MessageId,
		data: &[u8],
	) -> Result<(), PostErr> {
		// Split the message into fragments
		let fragment_blueprints = match fragment_blueprints(message_id, data, 0) {
			Some(fragment_blueprints)
				if fragment_blueprints.len() <=
					min(self.config.max_fragments_per_message, surbs.len()) =>
				fragment_blueprints,
			_ => return Err(PostErr::TooManyFragments),
		};

		// Grab the session and check there's room in the queue
		let session = post_session(&mut self.sessions, self.session_status, session_index)?;
		// TODO Something better than this
		if fragment_blueprints.len() > session.authored_packet_queue.remaining_capacity() {
			return Err(PostErr::NotEnoughSpaceInQueue)
		}

		// Generate the packets and push them into the queue
		for fragment_blueprint in fragment_blueprints {
			let mut packet = default_boxed_array();
			fragment_blueprint.write_except_surbs(mut_payload_data(&mut packet));
			let mixnode_index = complete_reply_packet(
				&mut packet,
				&surbs.pop().expect("Checked number of SURBs above"),
			)
			.ok_or(PostErr::BadSurb)?;
			let peer_id = session
				.topology
				.mixnode_index_to_peer_id(mixnode_index)
				.map_err(PostErr::Topology)?;
			session.authored_packet_queue.push(AddressedPacket { peer_id, packet });
		}

		Ok(())
	}

	pub fn take_invalidated(&mut self) -> Invalidated {
		let invalidated = self.invalidated;
		self.invalidated = Invalidated::empty();
		invalidated
	}
}
