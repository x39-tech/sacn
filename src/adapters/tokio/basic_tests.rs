//! Integration tests for the tokio adapter, driven over real loopback sockets.
//!
//! These bind an ephemeral port (rather than the fixed sACN port) so concurrent
//! tests stay isolated, and deliver data by unicasting to that port. The data
//! path through the core is exercised end-to-end without depending on multicast
//! delivery, which is unreliable across CI platforms.

use std::time::Duration as StdDuration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

use super::*;
use crate::packet::{DataPacket, Payload};
use crate::time::Duration;
use crate::types::Cid;

/// A generous bound on how long any single event should take to arrive.
const EVENT_TIMEOUT: StdDuration = StdDuration::from_secs(2);

fn cid(n: u8) -> Cid {
    Cid::from_bytes([n; 16])
}

/// Serializes a NULL-start-code data packet for `universe` carrying `values`.
fn data_packet(source: Cid, universe: u16, seq: u8, values: &[u8]) -> Vec<u8> {
    let packet = Packet {
        cid: source,
        payload: Payload::Data(DataPacket {
            source_name: "test source",
            priority: 100,
            sync_address: 0,
            sequence_number: crate::types::SequenceNumber::new(seq),
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

/// Binds a receiver to an ephemeral loopback port with the given config.
async fn bind_loopback(config: ReceiverConfig) -> BasicReceiver {
    BasicReceiver::bind_to("127.0.0.1:0".parse().unwrap(), config)
        .await
        .expect("bind loopback receiver")
}

/// Registers a universe in the core without performing any real multicast join,
/// so the data-path tests do not depend on the host's network interfaces.
fn listen_without_network(rx: &mut BasicReceiver, universe: Universe) {
    let now = rx.now();
    let outcome = rx
        .core
        .listen(now, universe)
        .expect("using heap-backed receiver in tests");
    if outcome.sampling_started {
        rx.pending
            .push_back(BasicReceiverEvent::SamplingStarted { universe });
    }
}

async fn next_event(rx: &mut BasicReceiver) -> BasicReceiverEvent {
    timeout(EVENT_TIMEOUT, rx.next_event())
        .await
        .expect("timed out waiting for event")
        .expect("receiver closed unexpectedly")
}

#[tokio::test]
async fn delivers_data_during_sampling() {
    let config = ReceiverConfig::new().with_sample_period(Duration::from_millis(300));
    let mut rx = bind_loopback(config).await;
    let universe = Universe::new(1).unwrap();
    listen_without_network(&mut rx, universe);
    let addr = rx.socket.local_addr().unwrap();

    // The opening event is the sampling period starting.
    assert_eq!(
        next_event(&mut rx).await,
        BasicReceiverEvent::SamplingStarted { universe }
    );

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&data_packet(cid(1), 1, 0, &[10, 20, 30]), addr)
        .await
        .unwrap();

    match next_event(&mut rx).await {
        BasicReceiverEvent::UniverseData(data) => {
            assert_eq!(data.universe, universe);
            assert_eq!(data.source.cid, cid(1));
            assert_eq!(data.values, [10, 20, 30]);
            assert_eq!(data.start_code, 0x00);
            assert!(data.is_sampling);
        }
        other => panic!("expected universe data, got {other:?}"),
    }
}

#[tokio::test]
async fn sampling_period_ends_on_its_timer() {
    let config = ReceiverConfig::new().with_sample_period(Duration::from_millis(50));
    let mut rx = bind_loopback(config).await;
    let universe = Universe::new(42).unwrap();
    listen_without_network(&mut rx, universe);

    let start = tokio::time::Instant::now();
    assert_eq!(
        next_event(&mut rx).await,
        BasicReceiverEvent::SamplingStarted { universe }
    );
    // With no traffic, the only thing that can happen is the sampling timer
    // firing, which the receiver's timer loop must surface on its own.
    assert_eq!(
        next_event(&mut rx).await,
        BasicReceiverEvent::SamplingEnded { universe }
    );
    let now = tokio::time::Instant::now();
    assert!(
        now - start >= Duration::from_millis(45),
        "sampling period {:?} should not be less than the configured time",
        now - start
    );
}

#[tokio::test]
async fn ignores_packets_for_unlistened_universes() {
    let config = ReceiverConfig::new().with_sample_period(Duration::from_millis(50));
    let mut rx = bind_loopback(config).await;
    let universe = Universe::new(1).unwrap();
    listen_without_network(&mut rx, universe);
    let addr = rx.socket.local_addr().unwrap();

    assert_eq!(
        next_event(&mut rx).await,
        BasicReceiverEvent::SamplingStarted { universe }
    );

    // A packet for a different universe must not produce universe data.
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&data_packet(cid(2), 99, 0, &[1, 2, 3]), addr)
        .await
        .unwrap();

    // The next event is the sampling period ending, not data for universe 99.
    assert_eq!(
        next_event(&mut rx).await,
        BasicReceiverEvent::SamplingEnded { universe }
    );
}

#[tokio::test]
async fn listen_enumerates_real_interfaces() {
    // Exercises the real interface-enumeration and multicast-join path. The host
    // may genuinely have no usable interface (e.g. a restricted sandbox), which
    // is a legitimate NoNetwork outcome rather than a test failure.
    let mut rx = bind_loopback(ReceiverConfig::new()).await;
    let universe = Universe::new(1).unwrap();
    match rx.listen(universe).await {
        Ok(()) => assert_eq!(
            next_event(&mut rx).await,
            BasicReceiverEvent::SamplingStarted { universe }
        ),
        Err(AdapterError::NoNetwork) => {}
        Err(other) => panic!("unexpected listen error: {other:?}"),
    }
}

#[tokio::test]
async fn stop_listening_reports_whether_it_was_listening() {
    let mut rx = bind_loopback(ReceiverConfig::new()).await;
    let universe = Universe::new(7).unwrap();
    listen_without_network(&mut rx, universe);

    assert!(rx.stop_listening(universe).await.unwrap());
    assert!(!rx.stop_listening(universe).await.unwrap());
}
