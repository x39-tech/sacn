//! Host tests for the embassy basic-receiver adapter.
//!
//! Not currently exercised on a live `embassy-net` stack; only pure helpers
//! are tested.

use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use embassy_net::udp::UdpMetadata;
use embassy_net::{IpAddress, IpEndpoint};

use crate::proto::SACN_PORT;

use super::super::BasicReceiverResources;
use super::source_addr;

crate::embassy_static_storage! {
    struct Caps {
        rx_universes: 4,
        rx_sources_per_universe: 8,
        rx_sync_addresses: 4,
        tx_universes: 0,
        tx_unicast_per_universe: 0,
    }
}

#[test]
fn source_addr_maps_ipv4_endpoint() {
    let meta = UdpMetadata::from(IpEndpoint::new(IpAddress::v4(192, 0, 2, 10), SACN_PORT));
    assert_eq!(
        source_addr(meta),
        SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), SACN_PORT))
    );
}

#[test]
fn source_addr_maps_ipv6_endpoint() {
    let addr = Ipv6Addr::new(0xff18, 0, 0, 0, 0, 0, 0x8300, 1);
    let meta = UdpMetadata::from(IpEndpoint::new(IpAddress::Ipv6(addr), SACN_PORT));
    assert_eq!(source_addr(meta), SocketAddr::from((addr, SACN_PORT)));
}

#[test]
fn basic_receiver_resources_are_const_constructible_for_static_storage() {
    static RESOURCES: static_cell::ConstStaticCell<BasicReceiverResources<Caps>> =
        static_cell::ConstStaticCell::new(Caps::embassy_basic_receiver_resources());
    let resources = RESOURCES.take();
    // A receiver only receives: its receive and datagram buffers default to a
    // single max-size packet, and its unused transmit ring is zero-sized.
    assert_eq!(resources.rx_buffer.len(), crate::packet::MAX_PACKET_SIZE);
    assert_eq!(resources.recv_buffer.len(), crate::packet::MAX_PACKET_SIZE);
}
