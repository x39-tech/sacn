//! Host tests for the embassy source-detector adapter.
//!
//! Not currently exercised on a live `embassy-net` stack; only pure helpers and
//! const construction are tested.

use crate::detector::{SourceDetectorEventRef, SourceDetectorPollEvent};
use crate::types::{Cid, SourceName};

use super::super::DetectorResources;
use super::expiry_ref;

crate::embassy_static_storage! {
    struct Caps {
        rx_universes: 0,
        rx_sources_per_universe: 0,
        rx_sync_addresses: 0,
        tx_universes: 0,
        tx_unicast_per_universe: 0,
        det_sources: 8,
        det_universes_per_source: 16,
    }
}

#[test]
fn expiry_ref_borrows_the_expired_source_name() {
    let event = SourceDetectorPollEvent::SourceExpired {
        cid: Cid::from_bytes([3; 16]),
        name: SourceName::from_str_lossy("gone"),
    };
    assert_eq!(
        expiry_ref(&event),
        SourceDetectorEventRef::SourceExpired {
            cid: Cid::from_bytes([3; 16]),
            name: "gone",
        }
    );
}

#[test]
fn detector_resources_are_const_constructible_for_static_storage() {
    static RESOURCES: static_cell::ConstStaticCell<DetectorResources<Caps>> =
        static_cell::ConstStaticCell::new(Caps::embassy_detector_resources());
    let resources = RESOURCES.take();
    // A detector only receives: its receive and datagram buffers default to a
    // single max-size packet.
    assert_eq!(resources.rx_buffer.len(), crate::packet::MAX_PACKET_SIZE);
    assert_eq!(resources.recv_buffer.len(), crate::packet::MAX_PACKET_SIZE);
}
