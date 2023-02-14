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

//! Mixnet cover packet generation.

use super::{
	boxed_packet::{AddressedPacket, BoxedPacket},
	config::Config,
	sphinx::build_cover_packet,
	topology::{LocalNetworkStatus, RouteGenerator, RouteKind, Topology, TopologyErr},
};
use arrayvec::ArrayVec;
use log::error;
use rand::{CryptoRng, Rng};

pub enum CoverKind {
	Drop,
	Loop,
}

pub fn gen_cover_packet(
	rng: &mut (impl Rng + CryptoRng),
	topology: &Topology,
	lns: &dyn LocalNetworkStatus,
	kind: CoverKind,
	config: &Config,
) -> Option<AddressedPacket> {
	if !config.gen_cover_packets {
		return None
	}

	let mut gen = || -> Result<AddressedPacket, TopologyErr> {
		// Generate route
		let route_generator = RouteGenerator::new(topology, lns);
		let route_kind = match kind {
			CoverKind::Drop => RouteKind::ToMixnode(route_generator.choose_destination_index(rng)?),
			CoverKind::Loop => RouteKind::Loop,
		};
		let mut targets = ArrayVec::new();
		let mut their_kx_publics = ArrayVec::new();
		let first_mixnode_index = route_generator.gen_route(
			&mut targets,
			&mut their_kx_publics,
			rng,
			route_kind,
			config.num_hops,
		)?;
		let peer_id = topology.mixnode_index_to_peer_id(first_mixnode_index)?;

		// Build packet
		let mut packet = BoxedPacket::default();
		build_cover_packet(packet.as_mut(), rng, &targets, &their_kx_publics, None);

		Ok(AddressedPacket { peer_id, packet })
	};

	match gen() {
		Ok(packet) => Some(packet),
		Err(err) => {
			error!(target: config.log_target, "Failed to generate cover packet: {err}");
			None
		},
	}
}
