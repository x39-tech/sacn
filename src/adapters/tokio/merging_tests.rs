//! Integration tests for the tokio merging-receiver adapter, driven over real
//! loopback sockets.
//!
//! Like the basic-adapter tests, these bind an ephemeral port and unicast to it,
//! so they exercise the full data path without depending on multicast delivery.

use std::time::Duration as StdDuration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

use super::*;
use crate::packet::{DataPacket, Payload};
use crate::time::Duration;
use crate::types::{Cid, SequenceNumber};

const EVENT_TIMEOUT: StdDuration = StdDuration::from_secs(2);

fn cid(n: u8) -> Cid {
    Cid::from_bytes([n; 16])
}

fn data_packet(source: Cid, universe: u16, seq: u8, priority: u8, values: &[u8]) -> Vec<u8> {
    let packet = Packet {
        cid: source,
        payload: Payload::Data(DataPacket {
            source_name: "test source",
            priority,
            sync_address: 0,
            sequence_number: SequenceNumber::new(seq),
            preview: false,
            stream_terminated: false,
            force_sync: false,
            universe,
            start_code: 0x00,
            values,
        }),
    };
    packet.to_vec().expect("serialize test packet")
}

async fn bind_loopback(config: ReceiverConfig) -> Receiver {
    Receiver::bind_to("127.0.0.1:0".parse().unwrap(), config)
        .await
        .expect("bind loopback receiver")
}

fn listen_without_network(rx: &mut Receiver, universe: Universe) {
    let now = rx.now();
    let outcome = rx
        .core
        .listen(now, universe)
        .expect("using heap-backed receiver in tests");
    if outcome.sampling_started {
        rx.pending
            .push_back(ReceiverEvent::SamplingStarted { universe });
    }
}

async fn next_event(rx: &mut Receiver) -> ReceiverEvent {
    timeout(EVENT_TIMEOUT, rx.next_event())
        .await
        .expect("timed out waiting for event")
        .expect("receiver closed unexpectedly")
}

#[tokio::test]
async fn emits_merged_data_after_sampling() {
    let config = ReceiverConfig::new().with_sample_period(Duration::from_millis(150));
    let mut rx = bind_loopback(config).await;
    let universe = Universe::new(1).unwrap();
    listen_without_network(&mut rx, universe);
    let addr = rx.socket.local_addr().unwrap();

    assert_eq!(
        next_event(&mut rx).await,
        ReceiverEvent::SamplingStarted { universe }
    );

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&data_packet(cid(1), 1, 0, 100, &[10, 20, 30]), addr)
        .await
        .unwrap();

    // Sampling must end before merged data is delivered.
    assert_eq!(
        next_event(&mut rx).await,
        ReceiverEvent::SamplingEnded { universe }
    );

    match next_event(&mut rx).await {
        ReceiverEvent::MergedData(data) => {
            assert_eq!(data.universe, universe);
            assert_eq!(&data.levels()[..3], &[10, 20, 30]);
            assert_eq!(data.active_sources().next().map(|s| s.cid), Some(cid(1)));
        }
        other => panic!("expected merged data, got {other:?}"),
    }
}

#[tokio::test]
async fn merges_two_sources_by_priority() {
    let config = ReceiverConfig::new().with_sample_period(Duration::from_millis(80));
    let mut rx = bind_loopback(config).await;
    let universe = Universe::new(5).unwrap();
    listen_without_network(&mut rx, universe);
    let addr = rx.socket.local_addr().unwrap();

    assert_eq!(
        next_event(&mut rx).await,
        ReceiverEvent::SamplingStarted { universe }
    );

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // Lower priority source.
    sender
        .send_to(&data_packet(cid(1), 5, 0, 100, &[200, 200]), addr)
        .await
        .unwrap();
    // Higher priority source wins both slots with its lower levels.
    sender
        .send_to(&data_packet(cid(2), 5, 0, 150, &[5, 6]), addr)
        .await
        .unwrap();

    // Drain events until we see a merged result reflecting both sources.
    let mut _saw_matching_event = false;
    loop {
        if let ReceiverEvent::MergedData(data) = next_event(&mut rx).await
            && data.active_sources().count() == 2
        {
            assert_eq!(&data.levels()[..2], &[5, 6]);
            let owner = data.source(data.owners()[0]).map(|s| s.cid);
            assert_eq!(owner, Some(cid(2)));
            _saw_matching_event = true;
            break;
        }
    }
    assert!(_saw_matching_event);
}

fn data_packet_sync(
    source: Cid,
    universe: u16,
    seq: u8,
    priority: u8,
    values: &[u8],
    sync_address: u16,
    force_sync: bool,
) -> Vec<u8> {
    let packet = Packet {
        cid: source,
        payload: Payload::Data(DataPacket {
            source_name: "test source",
            priority,
            sync_address,
            sequence_number: SequenceNumber::new(seq),
            preview: false,
            stream_terminated: false,
            force_sync,
            universe,
            start_code: 0x00,
            values,
        }),
    };
    packet.to_vec().expect("serialize test packet")
}

fn sync_packet_bytes(source: Cid, sync_address: u16) -> Vec<u8> {
    use crate::packet::SyncPacket;
    let packet = Packet {
        cid: source,
        payload: Payload::Sync(SyncPacket {
            sequence_number: SequenceNumber::new(0),
            sync_address,
        }),
    };
    packet.to_vec().expect("serialize sync packet")
}

#[tokio::test]
async fn synchronization_holds_and_releases_over_the_wire() {
    let config = ReceiverConfig::new()
        .with_sample_period(Duration::from_millis(80))
        .with_per_address_priority_handling(false);
    let mut rx = bind_loopback(config).await;
    let universe = Universe::new(1).unwrap();
    let sync_universe = 100u16;
    listen_without_network(&mut rx, universe);
    let addr = rx.socket.local_addr().unwrap();

    assert_eq!(
        next_event(&mut rx).await,
        ReceiverEvent::SamplingStarted { universe }
    );

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // Synchronized data before any sync is delivered live (startup window).
    sender
        .send_to(
            &data_packet_sync(cid(1), 1, 0, 100, &[10], sync_universe, false),
            addr,
        )
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut rx).await,
        ReceiverEvent::SamplingEnded { universe }
    );
    match next_event(&mut rx).await {
        ReceiverEvent::MergedData(data) => assert_eq!(data.levels()[0], 10),
        other => panic!("expected first live merged data, got {other:?}"),
    }

    // Activate the sync address (first sync releases nothing), then send a
    // withheld update followed by the releasing sync.
    sender
        .send_to(&sync_packet_bytes(cid(1), sync_universe), addr)
        .await
        .unwrap();
    sender
        .send_to(
            &data_packet_sync(cid(1), 1, 1, 100, &[42], sync_universe, false),
            addr,
        )
        .await
        .unwrap();
    sender
        .send_to(&sync_packet_bytes(cid(1), sync_universe), addr)
        .await
        .unwrap();

    // The withheld frame is only revealed by the second sync, delivered as a
    // coherent synchronized release.
    loop {
        if let ReceiverEvent::SyncMergedData(frames) = next_event(&mut rx).await
            && let Some(data) = frames.iter().find(|d| d.levels()[0] == 42)
        {
            assert_eq!(data.universe, universe);
            break;
        }
    }
}
