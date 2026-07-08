//! Integration tests for the tokio source-detector adapter, driven over real
//! loopback sockets.
//!
//! These bind an ephemeral port (rather than the fixed sACN port) so concurrent
//! tests stay isolated, and deliver discovery packets by unicasting to that
//! port, exercising the data path through the core without depending on
//! multicast delivery.

use std::time::Duration as StdDuration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

use super::*;
use crate::packet::{Packet, Payload, UniverseDiscoveryPacket, UniverseList};
use crate::time::Duration;
use crate::types::Cid;

/// A generous bound on how long any single event should take to arrive.
const EVENT_TIMEOUT: StdDuration = StdDuration::from_secs(2);

fn cid(n: u8) -> Cid {
    Cid::from_bytes([n; 16])
}

/// Serializes a single-page universe discovery packet listing `universes`.
fn discovery_packet(source: Cid, name: &str, universes: &[u16]) -> Vec<u8> {
    let bytes: Vec<u8> = universes.iter().flat_map(|u| u.to_be_bytes()).collect();
    let packet = Packet {
        cid: source,
        payload: Payload::UniverseDiscovery(UniverseDiscoveryPacket {
            source_name: name,
            page: 0,
            last_page: 0,
            universes: UniverseList::from_bytes(&bytes),
        }),
    };
    packet.to_vec().expect("serialize discovery packet")
}

/// Binds a detector to an ephemeral loopback port with the given config, without
/// joining any multicast group.
async fn bind_loopback(config: SourceDetectorConfig) -> SourceDetector {
    SourceDetector::bind_to("127.0.0.1:0".parse().unwrap(), config)
        .await
        .expect("bind loopback detector")
}

async fn next_event(d: &mut SourceDetector) -> SourceDetectorEvent {
    timeout(EVENT_TIMEOUT, d.next_event())
        .await
        .expect("timed out waiting for event")
        .expect("detector closed unexpectedly")
}

#[tokio::test]
async fn reports_a_discovered_source() {
    let mut d = bind_loopback(SourceDetectorConfig::new()).await;
    let addr = d.socket.local_addr().unwrap();

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&discovery_packet(cid(1), "src", &[1, 2, 3]), addr)
        .await
        .unwrap();

    match next_event(&mut d).await {
        SourceDetectorEvent::SourceUpdated {
            cid: c,
            name,
            universes,
        } => {
            assert_eq!(c, cid(1));
            assert_eq!(name, "src");
            assert_eq!(universes, vec![1, 2, 3]);
        }
        other => panic!("expected source updated, got {other:?}"),
    }
}

#[tokio::test]
async fn expires_a_silent_source() {
    let config = SourceDetectorConfig::new().with_source_timeout(Duration::from_millis(100));
    let mut d = bind_loopback(config).await;
    let addr = d.socket.local_addr().unwrap();

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&discovery_packet(cid(7), "gone", &[10]), addr)
        .await
        .unwrap();

    assert!(matches!(
        next_event(&mut d).await,
        SourceDetectorEvent::SourceUpdated { .. }
    ));

    // With no further traffic, the only thing that can happen is the expiry
    // timer firing, which the detector's timer loop must surface on its own.
    match next_event(&mut d).await {
        SourceDetectorEvent::SourceExpired { cid: c, name } => {
            assert_eq!(c, cid(7));
            assert_eq!(name, "gone");
        }
        other => panic!("expected source expired, got {other:?}"),
    }
}

#[tokio::test]
async fn ignores_non_discovery_packets() {
    use crate::packet::DataPacket;
    use crate::types::SequenceNumber;

    let config = SourceDetectorConfig::new().with_source_timeout(Duration::from_millis(100));
    let mut d = bind_loopback(config).await;
    let addr = d.socket.local_addr().unwrap();

    // Send a data packet (not discovery) followed by a valid discovery packet.
    let data = Packet {
        cid: cid(2),
        payload: Payload::Data(DataPacket {
            source_name: "data",
            priority: 100,
            sync_address: 0,
            sequence_number: SequenceNumber::new(0),
            preview: false,
            stream_terminated: false,
            force_sync: false,
            universe: 1,
            start_code: 0x00,
            values: &[1, 2, 3],
        }),
    }
    .to_vec()
    .unwrap();

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender.send_to(&data, addr).await.unwrap();
    sender
        .send_to(&discovery_packet(cid(3), "real", &[5]), addr)
        .await
        .unwrap();

    // The first event must come from the discovery packet; the data packet is
    // silently ignored rather than tracked as a source.
    match next_event(&mut d).await {
        SourceDetectorEvent::SourceUpdated { cid: c, .. } => assert_eq!(c, cid(3)),
        other => panic!("expected source updated for cid 3, got {other:?}"),
    }
    // Next event should be the source timing out
    match next_event(&mut d).await {
        SourceDetectorEvent::SourceExpired { cid: c, name } => {
            assert_eq!(c, cid(3));
            assert_eq!(name, "real");
        }
        other => panic!("expected source expired, got {other:?}"),
    }
}

#[tokio::test]
async fn bind_enumerates_real_interfaces() {
    // Exercises the real interface-enumeration and multicast-join path. The host
    // may genuinely have no usable interface (e.g. a restricted sandbox), which
    // is a legitimate NoNetwork outcome rather than a test failure.
    match SourceDetector::bind(SourceDetectorConfig::new()).await {
        Ok(_) | Err(AdapterError::NoNetwork) => {}
        Err(other) => panic!("unexpected bind error: {other:?}"),
    }
}
