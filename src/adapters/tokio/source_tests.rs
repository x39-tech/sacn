//! Integration tests for the tokio source adapter, driven over real loopback
//! sockets.
//!
//! Like the receiver adapter tests, these bind ephemeral ports and deliver over
//! unicast to loopback, which is reliable across CI platforms.

use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration as StdDuration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

use super::*;
use crate::packet::{Packet, Payload};
use crate::types::Cid;

const RECV_TIMEOUT: StdDuration = StdDuration::from_secs(2);

fn cid(n: u8) -> Cid {
    Cid::from_bytes([n; 16])
}

fn univ(n: u16) -> Universe {
    Universe::new(n).unwrap()
}

/// Binds a socket that stands in for a remote receiver, plus a source
/// configured to unicast to it.
async fn recv_socket_and_source(config: SourceConfig) -> (UdpSocket, Source, u16) {
    let recv_sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind wire");
    let port = recv_sock.local_addr().unwrap().port();
    let source = Source::bind(config).await.expect("bind source");
    (recv_sock, source, port)
}

/// Receives one datagram from the wire.
async fn recv_packet(wire: &UdpSocket) -> Vec<u8> {
    let mut buf = vec![0u8; crate::packet::MAX_PACKET_SIZE];
    let (len, _from) = timeout(RECV_TIMEOUT, wire.recv_from(&mut buf))
        .await
        .expect("timed out waiting for a packet")
        .expect("recv failed");
    buf.truncate(len);
    buf
}

#[tokio::test]
async fn unicast_data_reaches_the_wire() {
    let (wire, mut source, port) =
        recv_socket_and_source(SourceConfig::new(cid(1), "loopback source")).await;
    let universe = univ(1);
    source
        .add_universe_on(UniverseConfig::new(universe), &[][..])
        .unwrap();
    source.add_unicast_to_port(universe, "127.0.0.1".parse().unwrap(), port);
    source.update_levels(universe, &[10, 20, 30]);

    // One process tick sends the first packet to the unicast destination.
    let _deadline = source.process().await.unwrap();
    let bytes = recv_packet(&wire).await;

    let packet = Packet::parse(&bytes).expect("valid sACN packet");
    assert_eq!(packet.cid, cid(1));
    let Payload::Data(data) = packet.payload else {
        panic!("expected a data packet");
    };
    assert_eq!(data.universe, 1);
    assert_eq!(data.values, &[10, 20, 30]);
    assert_eq!(data.source_name, "loopback source");
    assert!(!data.stream_terminated);
}

#[tokio::test]
async fn unicast_destination_bookkeeping() {
    let (_wire, mut source, _port) = recv_socket_and_source(SourceConfig::new(cid(4), "uni")).await;
    let universe = univ(2);
    let addr = "127.0.0.1".parse().unwrap();

    // A universe must exist before it can take a unicast destination.
    assert!(!source.add_unicast(universe, addr));

    source
        .add_universe_on(UniverseConfig::new(universe), &[][..])
        .unwrap();
    // First add succeeds; a duplicate is rejected.
    assert!(source.add_unicast(universe, addr));
    assert!(!source.add_unicast(universe, addr));
    // The same address on a different port is an independent destination.
    assert!(source.add_unicast_to_port(universe, addr, 9000));
    assert!(!source.add_unicast_to_port(universe, addr, 9000));
    // Removing it succeeds once, then reports it is gone. The standard-port and
    // explicit-port destinations are removed independently.
    assert!(source.remove_unicast(universe, addr));
    assert!(!source.remove_unicast(universe, addr));
    assert!(source.remove_unicast_from_port(universe, addr, 9000));
    assert!(!source.remove_unicast_from_port(universe, addr, 9000));
}

#[tokio::test]
async fn sequence_numbers_advance_across_ticks() {
    let (wire, mut source, port) = recv_socket_and_source(SourceConfig::new(cid(2), "seq")).await;
    let universe = univ(5);
    source
        .add_universe_on(UniverseConfig::new(universe), &[][..])
        .unwrap();
    source.add_unicast_to_port(universe, "127.0.0.1".parse().unwrap(), port);
    source.update_levels(universe, &[0]);

    let mut last: Option<u8> = None;
    for _ in 0..3 {
        let deadline = source.process().await.unwrap();
        let bytes = recv_packet(&wire).await;
        let Payload::Data(data) = Packet::parse(&bytes).unwrap().payload else {
            panic!("expected data");
        };
        let seq = data.sequence_number.get();
        if let Some(prev) = last {
            assert_eq!(seq, prev.wrapping_add(1), "sequence advances by one");
        }
        last = Some(seq);
        if let Some(at) = deadline {
            tokio::time::sleep_until(at).await;
        }
    }
}

#[tokio::test]
async fn termination_sends_terminated_packets() {
    let (wire, mut source, port) = recv_socket_and_source(SourceConfig::new(cid(3), "term")).await;
    let universe = univ(9);
    source
        .add_universe_on(UniverseConfig::new(universe), &[][..])
        .unwrap();
    source.add_unicast_to_port(universe, "127.0.0.1".parse().unwrap(), port);
    source.update_levels(universe, &[200]);

    // Get it transmitting, then remove it.
    source.process().await.unwrap();
    let _ = recv_packet(&wire).await;

    source.remove_universe(universe);

    // The next three packets are stream-terminated.
    for _ in 0..3 {
        let deadline = source.process().await.unwrap();
        let bytes = recv_packet(&wire).await;
        let Payload::Data(data) = Packet::parse(&bytes).unwrap().payload else {
            panic!("expected data");
        };
        assert!(data.stream_terminated, "termination packet sets the flag");
        if let Some(at) = deadline {
            tokio::time::sleep_until(at).await;
        }
    }

    assert!(
        !source.has_universe(universe),
        "universe removed after termination"
    );
}

#[tokio::test]
async fn send_now_reaches_the_wire_in_sequence() {
    let (wire, mut source, port) = recv_socket_and_source(SourceConfig::new(cid(6), "adhoc")).await;
    let universe = univ(1);
    source
        .add_universe_on(UniverseConfig::new(universe), &[][..])
        .unwrap();
    source.add_unicast_to_port(universe, "127.0.0.1".parse().unwrap(), port);
    source.update_levels(universe, &[1, 2, 3]);

    // The scheduled level packet takes sequence 0.
    source.process().await.unwrap();
    let level = recv_packet(&wire).await;
    let Payload::Data(data) = Packet::parse(&level).unwrap().payload else {
        panic!("expected data");
    };
    assert_eq!(data.start_code, 0x00);
    assert_eq!(data.sequence_number.get(), 0);

    // An ad-hoc packet follows on the same universe, next in the shared sequence.
    source
        .send_now(universe, StartCode::new(0x55), &[7, 8])
        .await
        .unwrap();
    let adhoc = recv_packet(&wire).await;
    let Payload::Data(data) = Packet::parse(&adhoc).unwrap().payload else {
        panic!("expected data");
    };
    assert_eq!(data.start_code, 0x55);
    assert_eq!(data.values, &[7, 8]);
    assert_eq!(data.sequence_number.get(), 1);
}

#[tokio::test]
async fn send_now_rejects_reserved_start_code() {
    let (_wire, mut source, _port) =
        recv_socket_and_source(SourceConfig::new(cid(7), "adhoc")).await;
    let universe = univ(1);
    source
        .add_universe_on(UniverseConfig::new(universe), &[][..])
        .unwrap();
    let err = source
        .send_now(universe, StartCode::PAP, &[1])
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        AdapterError::Protocol(crate::Error::ReservedStartCode { start_code: 0xDD }),
    ));
}

/// Drains every datagram currently queued on `wire`, returning the set of
/// universes whose data packets arrived. Stops once the socket is idle.
async fn drain_universes(wire: &UdpSocket) -> BTreeSet<u16> {
    let mut seen = BTreeSet::new();
    let mut buf = vec![0u8; crate::packet::MAX_PACKET_SIZE];
    while let Ok(Ok((len, _))) =
        timeout(StdDuration::from_millis(200), wire.recv_from(&mut buf)).await
    {
        if let Ok(packet) = Packet::parse(&buf[..len]) {
            if let Payload::Data(data) = packet.payload {
                seen.insert(data.universe);
            }
        }
    }
    seen
}

#[tokio::test]
async fn cancelled_process_resumes_over_the_wire() {
    let (wire, mut source, port) =
        recv_socket_and_source(SourceConfig::new(cid(5), "cancel")).await;

    // More universes than tokio's per-poll cooperative budget (128 ready I/O
    // operations), so a single poll of `process` cannot drain them all.
    const N: u16 = 200;
    for n in 1..=N {
        let universe = univ(n);
        source
            .add_universe_on(UniverseConfig::new(universe), &[][..])
            .unwrap();
        source.add_unicast_to_port(universe, "127.0.0.1".parse().unwrap(), port);
        source.update_levels(universe, &[n as u8]);
    }

    // Poll the `process` future exactly once, then drop it. tokio's cooperative
    // budget forces `send_to` to yield partway through the drain, so this
    // abandons it mid-flight with a committed-but-unsent tail - the same shape as
    // a `process` future cancelled by a losing `select!` branch.
    {
        let mut fut = std::pin::pin!(source.process());
        std::future::poll_fn(|cx| {
            let _ = fut.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;
    }
    let mut seen = drain_universes(&wire).await;
    assert!(
        (seen.len() as u16) < N,
        "the cancelled poll should not have drained every universe (budget not exceeded?)",
    );

    // Resuming sends the abandoned tail (no fresh data is due yet); a second call
    // settles any resume round-trip.
    source.process().await.unwrap();
    source.process().await.unwrap();
    seen.extend(drain_universes(&wire).await);

    // Every universe reached the wire despite the mid-drain cancellation.
    assert_eq!(
        seen.len() as u16,
        N,
        "a universe was dropped by the cancellation"
    );
    assert_eq!(*seen.iter().next().unwrap(), 1);
    assert_eq!(*seen.iter().next_back().unwrap(), N);
}

#[tokio::test]
async fn synchronized_universe_emits_data_then_sync_on_the_wire() {
    let (wire, mut source, port) = recv_socket_and_source(
        SourceConfig::new(cid(7), "sync source").with_sync_delay(StdDuration::ZERO),
    )
    .await;
    let universe = univ(1);
    let sync_universe = univ(100);
    source
        .add_universe_on(
            UniverseConfig::new(universe).synchronized_on(sync_universe, OnSyncLoss::HoldLastLook),
            &[][..],
        )
        .unwrap();
    source.add_unicast_to_port(universe, "127.0.0.1".parse().unwrap(), port);
    source.update_levels(universe, &[1, 2, 3]);

    // One tick sends the data packet (advertising the sync address) and the
    // synchronization packet that releases it, since we configured a zero
    // sync delay.
    let _ = source.process().await.unwrap();
    let data_bytes = recv_packet(&wire).await;
    let Payload::Data(d) = Packet::parse(&data_bytes).unwrap().payload else {
        panic!("expected a data packet first");
    };
    assert_eq!(d.universe, 1);
    assert_eq!(d.sync_address, 100);

    let sync_bytes = recv_packet(&wire).await;
    let Payload::Sync(s) = Packet::parse(&sync_bytes).unwrap().payload else {
        panic!("expected a sync packet after the data");
    };
    assert_eq!(s.sync_address, 100);
}
