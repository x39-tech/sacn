//! Host tests for the embassy basic-receiver adapter.
//!
//! Not currently exercised on a live `embassy-net` stack; only pure helpers
//! are tested.

use super::super::ReceiverResources;

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
fn receiver_resources_are_const_constructible_for_static_storage() {
    static RESOURCES: static_cell::ConstStaticCell<ReceiverResources<Caps>> =
        static_cell::ConstStaticCell::new(Caps::embassy_receiver_resources());
    let resources = RESOURCES.take();
    assert_eq!(resources.rx_buffer.len(), crate::packet::MAX_PACKET_SIZE);
    assert_eq!(resources.recv_buffer.len(), crate::packet::MAX_PACKET_SIZE);
}
