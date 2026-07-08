//! End-to-end check that a [`static_storage!`](sacn::static_storage) policy
//! drives every core state machine through the public API with no allocator
//! behind its collections.

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use sacn::detector::SourceDetector;
use sacn::merger::DmxMerger;
use sacn::packet::Packet;
use sacn::receiver::{BasicReceiver, MergedPacketOutcome, PacketOutcome, Receiver, ReceiverConfig};
use sacn::source::{Route, Source, SourceConfig, UniverseConfig};
use sacn::time::{Duration, Instant};
use sacn::{Cid, NetintId, Priority, Universe};

sacn::static_storage! {
    /// A fixed-capacity, allocation-free storage policy for the round trip.
    pub struct Caps {
        rx_universes: 4,
        rx_sources_per_universe: 8,
        rx_sync_addresses: 8,
        tx_universes: 4,
        det_sources: 5,
        det_universes_per_source: 5,
    }
}

fn cid(n: u8) -> Cid {
    Cid::from_bytes([n; 16])
}

fn uni(n: u16) -> Universe {
    Universe::new(n).unwrap()
}

fn at(ms: u64) -> Instant {
    Instant::from_epoch(Duration::from_millis(ms))
}

fn addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5568)
}

/// A source transmitting `[4, 5, 6]` on universe 1, with the serialized packets
/// it emits at `t = 0` copied out for parsing.
fn source_with_data() -> (Source<Caps>, Vec<(Route, Vec<u8>)>) {
    let mut source: Source<Caps> = Source::with_config(SourceConfig::new(cid(9), "static"));
    assert!(source
        .add_universe(UniverseConfig::new(uni(1)))
        .expect("should have capacity"));
    source.update_levels(uni(1), &[4, 5, 6]);

    let mut packets = Vec::new();
    let mut poll = source.poll(at(0));
    while let Some(transmission) = poll.next_transmission() {
        packets.push((transmission.route, transmission.data.to_vec()));
    }
    (source, packets)
}

/// The serialized bytes of the first emitted packet whose route matches.
fn find(packets: &[(Route, Vec<u8>)], want: impl Fn(&Route) -> bool) -> &[u8] {
    packets
        .iter()
        .find(|(route, _)| want(route))
        .map(|(_, data)| data.as_slice())
        .expect("the source emitted the expected packet")
}

#[test]
fn merger_merges_htp() {
    let mut merger: DmxMerger<Caps> = DmxMerger::default();
    let a = merger.add_source().unwrap();
    let b = merger.add_source().unwrap();
    merger.update_universe_priority(a, Priority::new(100).unwrap());
    merger.update_universe_priority(b, Priority::new(100).unwrap());
    merger.update_levels(a, &[10, 200, 5]);
    merger.update_levels(b, &[100, 50, 5]);
    assert_eq!(&merger.output().levels()[..3], &[100, 200, 5]);
}

#[test]
fn source_round_trips_to_basic_receiver() {
    let (_source, packets) = source_with_data();
    let bytes = find(
        &packets,
        |route| matches!(route, Route::Universe(u) if *u == uni(1)),
    );
    let packet = Packet::parse(bytes).expect("the source's own bytes parse");

    let mut rx: BasicReceiver<Caps> = BasicReceiver::with_config(ReceiverConfig::default());
    rx.listen(at(0), uni(1))
        .expect("within Caps universe capacity");

    match rx.handle_packet(at(0), addr(), NetintId::UNKNOWN, &packet) {
        PacketOutcome::Data {
            universe,
            data: Some(data),
            ..
        } => {
            assert_eq!(universe, uni(1));
            assert_eq!(&data.values[..3], &[4, 5, 6]);
        }
        other => panic!("expected delivered data, got {other:?}"),
    }
}

#[test]
fn source_round_trips_to_merging_receiver() {
    let (_source, packets) = source_with_data();
    let bytes = find(
        &packets,
        |route| matches!(route, Route::Universe(u) if *u == uni(1)),
    );
    let packet = Packet::parse(bytes).expect("the source's own bytes parse");

    let mut rx: Receiver<Caps> = Receiver::with_config(ReceiverConfig::default());
    rx.listen(at(0), uni(1))
        .expect("within Caps universe capacity");

    let accepted = matches!(
        rx.handle_packet(at(0), addr(), NetintId::UNKNOWN, &packet),
        MergedPacketOutcome::Data { universe, .. } if universe == uni(1),
    );
    assert!(accepted, "the merging receiver should accept the packet");

    // End the 1500 ms sampling period (still inside the 2500 ms loss timeout, so
    // the source is not dropped): draining the poll commits the sampling-end, and
    // the accumulated frame becomes the live result.
    let mut outcome = rx.poll(at(1600));
    while outcome.next_event().is_some() {}
    let merged = rx
        .merged(uni(1))
        .expect("a live merged frame after sampling");
    assert_eq!(&merged.levels()[..3], &[4, 5, 6]);
}

#[test]
fn source_round_trips_to_detector() {
    let (_source, packets) = source_with_data();
    let bytes = find(&packets, |route| matches!(route, Route::Discovery));
    let packet = Packet::parse(bytes).expect("the source's own discovery bytes parse");

    let mut detector: SourceDetector<Caps> = SourceDetector::with_config(Default::default());
    let outcome = detector.handle_packet(at(0), &packet);
    let updated = outcome.updated.expect("a completed universe list");
    assert_eq!(updated.cid, cid(9));
    assert_eq!(updated.universes, &[1]);
}
