//! Unit tests for the merging receiver.
//!
//! The tests uses a fixed capacity policy equal to the maximum number of
//! relevant resources that group reaches. This tests the boundary conditions
//! for the fixed-capacity storage at the same time as the actual test logic is
//! exercised.

use super::*;

use crate::merger::SlotOwner;
use crate::packet::{DataPacket, Packet, Payload};
use crate::time::Duration;
use crate::types::{Cid, NetintId, SequenceNumber};
use crate::{static_storage, ReceiverStorage};

use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

// --- test storage policies --------------------------------------------------

static_storage! {
    struct TestCaps {
        // For releasing 2 universes together under sync
        rx_universes: 2,
        // HTP merge and other multi-source tests
        rx_sources_per_universe: 2,
        // Sync tests
        rx_sync_addresses: 1,
        tx_universes: 0,
        det_sources: 0,
        det_universes_per_source: 0
    }
}

type Receiver = super::Receiver<TestCaps>;

impl Receiver {
    fn new(config: ReceiverConfig) -> Self {
        Self::with_config(config)
    }
}

// --- Helpers -----------------------------------------------------------------

fn instant(ms: u64) -> Instant {
    Instant::from_epoch(Duration::from_millis(ms))
}

fn cid(n: u8) -> Cid {
    Cid::from_bytes([n; 16])
}

fn uni(n: u16) -> Universe {
    Universe::new(n).unwrap()
}

fn test_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 5568)
}

/// Builds and feeds a data packet, collecting the owned events it produced.
#[allow(clippy::too_many_arguments)]
fn feed(
    rx: &mut Receiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
    start_code: u8,
    priority: u8,
    values: &[u8],
    terminated: bool,
) -> Vec<ReceiverEvent> {
    let packet = Packet {
        cid: source,
        payload: Payload::Data(DataPacket {
            source_name: "src",
            priority,
            sync_address: 0,
            sequence_number: SequenceNumber::new(seq),
            preview: false,
            stream_terminated: terminated,
            force_sync: false,
            universe,
            start_code,
            values,
        }),
    };
    let mut events = Vec::new();
    rx.handle_packet(instant(ms), test_addr(), NetintId::UNKNOWN, &packet)
        .for_each_owned(|event| events.push(event));
    events
}

/// Feeds a NULL-start-code (levels) packet.
fn dmx(
    rx: &mut Receiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
    priority: u8,
    values: &[u8],
) -> Vec<ReceiverEvent> {
    feed(
        rx,
        ms,
        source,
        universe,
        seq,
        DMX_NULL_START_CODE,
        priority,
        values,
        false,
    )
}

/// Feeds a per-address-priority packet.
fn pap(
    rx: &mut Receiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
    values: &[u8],
) -> Vec<ReceiverEvent> {
    feed(
        rx,
        ms,
        source,
        universe,
        seq,
        PAP_START_CODE,
        100,
        values,
        false,
    )
}

/// Drives `poll` and resolves its narrowed [`ReceiverPollEvent`]s into the owned
/// [`ReceiverEvent`] form the assertions are written against, looking up each
/// `MergedDataChanged` through the outcome.
fn poll(rx: &mut Receiver, ms: u64) -> Vec<ReceiverEvent> {
    let mut outcome = rx.poll(instant(ms));
    let mut events = Vec::new();
    while let Some(event) = outcome.next_event() {
        match event {
            ReceiverPollEvent::SamplingEnded { universe } => {
                events.push(ReceiverEvent::SamplingEnded { universe });
            }
            ReceiverPollEvent::MergedDataChanged { universe } => {
                if let Some(merged) = outcome.merged(universe) {
                    events.push(ReceiverEvent::MergedData(merged.to_owned()));
                }
            }
            ReceiverPollEvent::SourcesLost { universe, sources } => {
                events.push(ReceiverEvent::SourcesLost {
                    universe,
                    sources: sources.to_vec(),
                });
            }
        }
    }
    events
}

fn merged(events: &[ReceiverEvent]) -> Option<&MergedData> {
    events.iter().find_map(|e| match e {
        ReceiverEvent::MergedData(m) => Some(m),
        ReceiverEvent::SyncMergedData(frames) => frames.first(),
        _ => None,
    })
}

fn has_sampling_ended(events: &[ReceiverEvent], universe: Universe) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ReceiverEvent::SamplingEnded { universe: u } if *u == universe))
}

/// Returns the CID owning `slot` in a merged result, or `None`.
fn owner_cid(m: &MergedData, slot: usize) -> Option<Cid> {
    m.source(m.owners()[slot]).map(|s| s.cid)
}

/// A receiver with a short sampling period for fast tests.
fn receiver(sample_ms: u64) -> Receiver {
    Receiver::new(ReceiverConfig::new().with_sample_period(Duration::from_millis(sample_ms)))
}

fn listen(rx: &mut Receiver, ms: u64, universe: Universe) {
    rx.listen(instant(ms), universe)
        .expect("within the test's universe capacity");
}

// --- Tests -------------------------------------------------------------------

#[test]
fn no_merged_data_during_sampling() {
    let mut rx = receiver(100);
    listen(&mut rx, 0, uni(1));

    // Feeding a source during sampling updates the merge but emits nothing.
    let events = dmx(&mut rx, 0, cid(1), 1, 0, 100, &[5, 6, 7]);
    assert!(merged(&events).is_none());
}

#[test]
fn sampling_end_emits_merged_htp() {
    let mut rx = receiver(100);
    listen(&mut rx, 0, uni(1));

    // Two equal-priority sources; HTP picks the highest level per slot.
    dmx(&mut rx, 0, cid(1), 1, 0, 100, &[10, 200]);
    dmx(&mut rx, 0, cid(2), 1, 0, 100, &[100, 100]);

    let events = poll(&mut rx, 200);
    assert!(has_sampling_ended(&events, uni(1)));
    let m = merged(&events).expect("merged data after sampling");
    assert_eq!(m.universe, uni(1));
    assert_eq!(&m.levels()[..2], &[100, 200]);
    // Slot 0: source 2 wins the higher level; slot 1: source 1.
    assert_eq!(owner_cid(m, 0), Some(cid(2)));
    assert_eq!(owner_cid(m, 1), Some(cid(1)));
    assert_eq!(m.levels().len(), 512);

    let mut sources: Vec<Cid> = m.active_sources().map(|s| s.cid).collect();
    sources.sort();
    assert_eq!(sources, [cid(1), cid(2)]);
}

#[test]
fn higher_priority_wins() {
    let mut rx = receiver(100);
    listen(&mut rx, 0, uni(1));

    dmx(&mut rx, 0, cid(1), 1, 0, 100, &[200]);
    dmx(&mut rx, 0, cid(2), 1, 0, 150, &[10]);

    let m = merged(&poll(&mut rx, 200)).expect("merged data").clone();
    // Source 2 has the higher priority, so its (lower) level wins.
    assert_eq!(m.levels()[0], 10);
    assert_eq!(m.priorities()[0], 150);
    assert_eq!(owner_cid(&m, 0), Some(cid(2)));
}

#[test]
fn per_address_priority_steers_ownership() {
    let mut rx = receiver(100);
    listen(&mut rx, 0, uni(1));

    // Source 1: plain levels at universe priority 100.
    dmx(&mut rx, 0, cid(1), 1, 0, 100, &[100, 100]);
    // Source 2: PAP raises slot 0 to 200 and disclaims slot 1 (PAP 0).
    pap(&mut rx, 0, cid(2), 1, 0, &[200, 0]);
    dmx(&mut rx, 0, cid(2), 1, 1, 100, &[50, 50]);

    let m = merged(&poll(&mut rx, 200)).expect("merged data").clone();
    // Slot 0: source 2's PAP (200) beats source 1's universe priority (100).
    // Slot 1: source 2 disclaims it, so source 1 wins.
    assert_eq!(&m.levels()[..2], &[50, 100]);
    assert_eq!(&m.priorities()[..2], &[200, 100]);
    assert_eq!(owner_cid(&m, 0), Some(cid(2)));
    assert_eq!(owner_cid(&m, 1), Some(cid(1)));

    let s2 = m
        .active_sources()
        .find(|s| s.cid == cid(2))
        .expect("source 2 active");
    assert!(s2.per_address_priority_active);
}

#[test]
fn post_sampling_packet_emits_merged_immediately() {
    // With PAP handling off, a new source's levels are not withheld, so the
    // first post-sampling packet yields a merged result right away.
    let mut rx = Receiver::new(
        ReceiverConfig::new()
            .with_sample_period(Duration::from_millis(50))
            .with_per_address_priority_handling(false),
    );
    listen(&mut rx, 0, uni(1));
    // End the (sourceless) sampling period.
    assert!(merged(&poll(&mut rx, 100)).is_none());

    let events = dmx(&mut rx, 100, cid(1), 1, 0, 100, &[7, 8]);
    let m = merged(&events).expect("merged immediately post-sampling");
    assert_eq!(&m.levels()[..2], &[7, 8]);
    assert_eq!(owner_cid(m, 0), Some(cid(1)));
}

#[test]
fn source_loss_updates_merge() {
    let mut rx = Receiver::new(
        ReceiverConfig::new()
            .with_sample_period(Duration::from_millis(50))
            .with_per_address_priority_handling(false),
    );
    listen(&mut rx, 0, uni(1));
    poll(&mut rx, 100); // end sampling

    dmx(&mut rx, 100, cid(1), 1, 0, 100, &[123]);

    // Source falls silent; after the 2.5s loss timeout it is reported lost and
    // the merge updates to empty.
    let events = poll(&mut rx, 100 + 2500);
    assert!(events
        .iter()
        .any(|e| matches!(e, ReceiverEvent::SourcesLost { .. })));
    let m = merged(&events).expect("merged after loss");
    assert_eq!(m.levels()[0], 0);
    assert_eq!(m.active_sources().count(), 0);
    assert!(m.owners()[0].is_none());
}

#[test]
fn alternate_start_code_passes_through() {
    let mut rx = Receiver::new(
        ReceiverConfig::new()
            .with_sample_period(Duration::from_millis(50))
            .with_allowed_start_codes(&[0x00, 0xDD, 0x50]),
    );
    listen(&mut rx, 0, uni(1));

    let events = feed(&mut rx, 0, cid(1), 1, 0, 0x50, 100, &[1, 2, 3], false);
    assert!(merged(&events).is_none());
    let data = events
        .iter()
        .find_map(|e| match e {
            ReceiverEvent::UniverseData(d) => Some(d),
            _ => None,
        })
        .expect("passthrough universe data");
    assert_eq!(data.start_code, 0x50);
    assert_eq!(data.values, [1, 2, 3]);
}

#[test]
fn pap_lost_reverts_to_universe_priority() {
    let mut rx = receiver(50);
    listen(&mut rx, 0, uni(1));

    // Source sends PAP then levels during sampling.
    pap(&mut rx, 0, cid(1), 1, 0, &[200]);
    dmx(&mut rx, 0, cid(1), 1, 1, 100, &[40]);
    let m = merged(&poll(&mut rx, 100))
        .expect("merged at sampling end")
        .clone();
    assert_eq!(m.priorities()[0], 200);

    // PAP stops. A NULL packet after the PAP timeout (2.5s past the last PAP)
    // reverts the source to its universe priority.
    let events = dmx(&mut rx, 2600, cid(1), 1, 2, 100, &[40]);
    assert!(events
        .iter()
        .any(|e| matches!(e, ReceiverEvent::SourcePapLost { .. })));
    let m = merged(&events).expect("merged after pap lost");
    assert_eq!(m.priorities()[0], 100);
    assert_eq!(m.levels()[0], 40);
    let s = m
        .active_sources()
        .find(|s| s.cid == cid(1))
        .expect("source still active");
    assert!(!s.per_address_priority_active);
}

#[test]
fn stop_listening_discards_merge() {
    let mut rx = receiver(50);
    listen(&mut rx, 0, uni(1));
    dmx(&mut rx, 0, cid(1), 1, 0, 100, &[1]);

    assert!(rx.stop_listening(uni(1)).was_listening);

    // After stopping, packets for the universe are ignored.
    let events = dmx(&mut rx, 10, cid(1), 1, 1, 100, &[2]);
    assert!(events.is_empty());
}

#[test]
fn sampling_started_comes_from_listen() {
    let mut rx = receiver(50);
    let outcome = rx
        .listen(instant(0), uni(1))
        .expect("within the test's universe capacity");
    assert!(outcome.sampling_started);
}

#[test]
fn listen_reports_a_full_universe_table() {
    let mut rx = receiver(50);
    let cap = <TestCaps as ReceiverStorage>::Universes::CAPACITY as u16;
    for u in 1..cap + 1 {
        assert!(
            rx.listen(instant(0), uni(u))
                .expect("universe within capacity fits")
                .sampling_started
        );
    }

    assert_eq!(rx.listen(instant(0), uni(cap + 1)), Err(Error::NoCapacity));
    assert!(rx.merged(uni(cap + 1)).is_none());
}

#[test]
fn sync_reports_a_full_sync_address_table() {
    let mut rx = receiver(50);
    let cap = <TestCaps as ReceiverStorage>::Universes::CAPACITY as u8;
    for addr in 1..cap + 1 {
        assert!(sync(&mut rx, 0, cid(addr), 100).is_empty());
    }
    assert_eq!(
        sync(&mut rx, 0, cid(cap + 1), 200),
        alloc::vec![ReceiverEvent::SyncLimitExceeded { sync_address: 200 }],
    );
}

#[test]
fn owned_round_trips_through_borrowed() {
    let mut rx = receiver(100);
    listen(&mut rx, 0, uni(1));
    dmx(&mut rx, 0, cid(1), 1, 0, 100, &[9, 9]);

    let events = poll(&mut rx, 200);
    let m = merged(&events).expect("merged data");
    // Owned resolution mirrors borrowed resolution.
    let owner = m.owners()[0];
    assert_eq!(m.source(owner).map(|s| s.cid), Some(cid(1)));
}

// --- Synchronization ---------------------------------------------------------

use crate::packet::SyncPacket;

/// Feeds a NULL-start-code (levels) packet carrying a synchronization address
/// and Force_Synchronization bit.
#[allow(clippy::too_many_arguments)]
fn dmx_sync(
    rx: &mut Receiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
    priority: u8,
    values: &[u8],
    sync_address: u16,
    force_sync: bool,
) -> Vec<ReceiverEvent> {
    let packet = Packet {
        cid: source,
        payload: Payload::Data(DataPacket {
            source_name: "src",
            priority,
            sync_address,
            sequence_number: SequenceNumber::new(seq),
            preview: false,
            stream_terminated: false,
            force_sync,
            universe,
            start_code: DMX_NULL_START_CODE,
            values,
        }),
    };
    let mut events = Vec::new();
    rx.handle_packet(instant(ms), test_addr(), NetintId::UNKNOWN, &packet)
        .for_each_owned(|event| events.push(event));
    events
}

/// Feeds a universe synchronization packet on `sync_address`.
fn sync(rx: &mut Receiver, ms: u64, source: Cid, sync_address: u16) -> Vec<ReceiverEvent> {
    let packet = Packet {
        cid: source,
        payload: Payload::Sync(SyncPacket {
            sequence_number: SequenceNumber::new(0),
            sync_address,
        }),
    };
    let mut events = Vec::new();
    rx.handle_packet(instant(ms), test_addr(), NetintId::UNKNOWN, &packet)
        .for_each_owned(|event| events.push(event));
    events
}

/// Short sampling period and per-address-priority handling off, so a new
/// source's first NULL levels are delivered immediately rather than withheld
/// for the PAP wait.
fn sync_receiver(sample_ms: u64) -> Receiver {
    Receiver::new(
        ReceiverConfig::new()
            .with_sample_period(Duration::from_millis(sample_ms))
            .with_per_address_priority_handling(false),
    )
}

fn all_merged(events: &[ReceiverEvent]) -> Vec<&MergedData> {
    events
        .iter()
        .flat_map(|e| match e {
            ReceiverEvent::MergedData(m) => core::slice::from_ref(m),
            ReceiverEvent::SyncMergedData(frames) => frames.as_slice(),
            _ => &[][..],
        })
        .collect()
}

fn end_sampling(rx: &mut Receiver, ms: u64) {
    let _ = poll(rx, ms);
}

#[test]
fn synced_data_is_live_until_first_sync() {
    let mut rx = sync_receiver(50);
    listen(&mut rx, 0, uni(1));
    end_sampling(&mut rx, 100);

    // A synchronized data packet with no sync yet seen is delivered live.
    let e = dmx_sync(&mut rx, 110, cid(1), 1, 0, 100, &[7], 100, false);
    assert_eq!(merged(&e).expect("live before first sync").levels()[0], 7);
}

#[test]
fn sync_holds_then_releases_the_frame() {
    let mut rx = sync_receiver(50);
    listen(&mut rx, 0, uni(1));
    end_sampling(&mut rx, 100);
    dmx_sync(&mut rx, 110, cid(1), 1, 0, 100, &[7], 100, false);

    // The first sync just activates the address; it releases nothing.
    let e = sync(&mut rx, 120, cid(1), 100);
    assert!(merged(&e).is_none());

    // Subsequent synchronized data is now withheld: output stays frozen at 7.
    let e = dmx_sync(&mut rx, 130, cid(1), 1, 1, 100, &[9], 100, false);
    assert!(merged(&e).is_none());
    assert_eq!(rx.merged(uni(1)).unwrap().levels()[0], 7);

    // The next sync latches the accumulated frame.
    let e = sync(&mut rx, 140, cid(1), 100);
    let m = merged(&e).expect("frame released on sync");
    assert_eq!(m.universe, uni(1));
    assert_eq!(m.levels()[0], 9);
}

#[test]
fn one_sync_releases_a_multi_universe_group() {
    let mut rx = sync_receiver(50);
    listen(&mut rx, 0, uni(1));
    listen(&mut rx, 0, uni(2));
    end_sampling(&mut rx, 100);

    // Prime both universes live, then activate the shared address.
    dmx_sync(&mut rx, 110, cid(1), 1, 0, 100, &[1], 100, false);
    dmx_sync(&mut rx, 110, cid(1), 2, 0, 100, &[2], 100, false);
    sync(&mut rx, 120, cid(1), 100);

    // New data for both universes is withheld until the sync.
    assert!(merged(&dmx_sync(
        &mut rx,
        130,
        cid(1),
        1,
        1,
        100,
        &[10],
        100,
        false
    ))
    .is_none());
    assert!(merged(&dmx_sync(
        &mut rx,
        130,
        cid(1),
        2,
        1,
        100,
        &[20],
        100,
        false
    ))
    .is_none());

    // One sync releases both universes coherently.
    let e = sync(&mut rx, 140, cid(1), 100);
    let released = all_merged(&e);
    assert_eq!(released.len(), 2);
    let u1 = released.iter().find(|m| m.universe == uni(1)).unwrap();
    let u2 = released.iter().find(|m| m.universe == uni(2)).unwrap();
    assert_eq!(u1.levels()[0], 10);
    assert_eq!(u2.levels()[0], 20);
}

#[test]
fn force_sync_false_freezes_on_sync_loss() {
    let mut rx = sync_receiver(50);
    listen(&mut rx, 0, uni(1));
    end_sampling(&mut rx, 100);
    dmx_sync(&mut rx, 110, cid(1), 1, 0, 100, &[7], 100, false);
    sync(&mut rx, 120, cid(1), 100);
    // A withheld update the sync never released; keeps the source alive too.
    dmx_sync(&mut rx, 200, cid(1), 1, 1, 100, &[9], 100, false);

    // Sync times out (120 + 2500 = 2620) while the source is still alive
    // (200 + 2500 = 2700). Under !force_sync the output stays frozen at 7.
    let e = poll(&mut rx, 2650);
    assert!(merged(&e).is_none());
    assert_eq!(rx.merged(uni(1)).unwrap().levels()[0], 7);

    // Additional live data while sync is dead is withheld and not delivered.
    // Previous levels remain.
    let e = dmx_sync(&mut rx, 2700, cid(1), 1, 2, 100, &[42], 100, false);
    assert!(merged(&e).is_none(), "HoldLastLook must keep withholding");
    assert_eq!(rx.merged(uni(1)).unwrap().levels()[0], 7);

    // When sync resumes, the first sync only re-arms the address. It takes
    // a second sync packet to actually release the new data.
    let e = sync(&mut rx, 2750, cid(1), 100);
    assert!(all_merged(&e).is_empty());
    assert_eq!(rx.merged(uni(1)).unwrap().levels()[0], 7);
    let e = dmx_sync(&mut rx, 2760, cid(1), 1, 3, 100, &[55], 100, false);
    assert!(merged(&e).is_none());
    let e = sync(&mut rx, 2770, cid(1), 100);
    assert_eq!(all_merged(&e)[0].levels()[0], 55);
}

#[test]
fn force_sync_true_resumes_on_sync_loss() {
    let mut rx = sync_receiver(50);
    listen(&mut rx, 0, uni(1));
    end_sampling(&mut rx, 100);
    // force_sync = 1 -> RevertToLive.
    dmx_sync(&mut rx, 110, cid(1), 1, 0, 100, &[7], 100, true);
    sync(&mut rx, 120, cid(1), 100);
    dmx_sync(&mut rx, 200, cid(1), 1, 1, 100, &[9], 100, true);

    // On sync loss the withheld frame is published and live output resumes.
    let e = poll(&mut rx, 2650);
    let m = merged(&e).expect("revert publishes the accumulated frame");
    assert_eq!(m.levels()[0], 9);

    // Further data now flows live again.
    let e = dmx_sync(&mut rx, 2660, cid(1), 1, 2, 100, &[11], 100, true);
    assert_eq!(merged(&e).expect("live after revert").levels()[0], 11);
}

#[test]
fn merged_change_collapses_within_a_poll() {
    // A single poll can change a universe's merged output in independent ways -
    // here a source loss and a synchronization-loss revert land in the same poll.
    // We want exactly one `MergedDataChanged` event to be emitted per poll
    // regardless of how many reasons it changed.
    let mut rx = sync_receiver(50);
    listen(&mut rx, 0, uni(1));
    end_sampling(&mut rx, 100);

    // Two sources on universe 1, both synchronized on address 100 with
    // force_sync = RevertToLive, so the universe withholds while 100 is active.
    dmx_sync(&mut rx, 150, cid(2), 1, 0, 100, &[5], 100, true);
    dmx_sync(&mut rx, 200, cid(1), 1, 0, 100, &[9], 100, true);
    sync(&mut rx, 150, cid(1), 100);

    // At 2650 two changes coincide: cid(2)'s stream is lost (150 + 2500) and sync
    // address 100 times out (150 + 2500), reverting the universe to live. cid(1)
    // (200 + 2500 = 2700) is still alive and becomes the live winner.
    let events = poll(&mut rx, 2650);

    let sources_lost = events
        .iter()
        .filter(|e| matches!(e, ReceiverEvent::SourcesLost { .. }))
        .count();
    let merged_changes = events
        .iter()
        .filter(|e| matches!(e, ReceiverEvent::MergedData(_)))
        .count();
    assert_eq!(sources_lost, 1, "the lost source is reported once");
    assert_eq!(
        merged_changes, 1,
        "the two concurrent changes collapse into a single MergedDataChanged"
    );
    assert_eq!(
        merged(&events)
            .expect("a live frame after the revert")
            .levels()[0],
        9,
        "the surviving source wins the reverted-to-live frame"
    );
}

#[test]
fn mixed_sources_fall_back_to_live() {
    let mut rx = sync_receiver(50);
    listen(&mut rx, 0, uni(1));
    end_sampling(&mut rx, 100);

    // One source syncs on 100, another sends unsynchronized on the same universe:
    // they disagree, so the universe is delivered live regardless of the address.
    dmx_sync(&mut rx, 110, cid(1), 1, 0, 100, &[7], 100, false);
    dmx_sync(&mut rx, 111, cid(2), 1, 0, 90, &[0], 0, false);
    sync(&mut rx, 120, cid(1), 100);

    let e = dmx_sync(&mut rx, 130, cid(1), 1, 1, 100, &[9], 100, false);
    assert_eq!(merged(&e).expect("live under disagreement").levels()[0], 9);
}

#[test]
fn disabled_synchronization_delivers_live() {
    let mut rx = Receiver::new(
        ReceiverConfig::new()
            .with_sample_period(Duration::from_millis(50))
            .with_per_address_priority_handling(false)
            .with_synchronization(false),
    );
    listen(&mut rx, 0, uni(1));
    end_sampling(&mut rx, 100);

    dmx_sync(&mut rx, 110, cid(1), 1, 0, 100, &[7], 100, false);
    // A sync packet is dropped entirely
    let e = sync(&mut rx, 120, cid(1), 100);
    assert!(e.is_empty());
    let e = dmx_sync(&mut rx, 130, cid(1), 1, 1, 100, &[9], 100, false);
    assert_eq!(merged(&e).expect("live when disabled").levels()[0], 9);
    assert!(rx.sync_group_interest().next().is_none());
}

#[test]
fn sync_group_interest_tracks_declared_addresses() {
    let mut rx = sync_receiver(50);
    listen(&mut rx, 0, uni(1));
    end_sampling(&mut rx, 100);
    dmx_sync(&mut rx, 110, cid(1), 1, 0, 100, &[7], 100, false);
    assert!(rx.sync_group_interest().any(|u| u == uni(100)));
}

// --- Synchronization x per-address priority ----------------------------------

/// Feeds a per-address-priority packet carrying a synchronization address.
fn pap_sync(
    rx: &mut Receiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
    values: &[u8],
    sync_address: u16,
) -> Vec<ReceiverEvent> {
    let packet = Packet {
        cid: source,
        payload: Payload::Data(DataPacket {
            source_name: "src",
            priority: 100,
            sync_address,
            sequence_number: SequenceNumber::new(seq),
            preview: false,
            stream_terminated: false,
            force_sync: false,
            universe,
            start_code: PAP_START_CODE,
            values,
        }),
    };
    let mut events = Vec::new();
    rx.handle_packet(instant(ms), test_addr(), NetintId::UNKNOWN, &packet)
        .for_each_owned(|event| events.push(event));
    events
}

#[test]
fn synchronized_pap_and_levels_release_together() {
    let mut rx = receiver(50);
    listen(&mut rx, 0, uni(1));
    dmx_sync(&mut rx, 0, cid(1), 1, 0, 100, &[50, 60], 100, false);
    pap_sync(&mut rx, 0, cid(1), 1, 1, &[0, 200], 100);
    let e = poll(&mut rx, 100);
    let m = merged(&e).expect("first frame after sampling");
    // Slot 0 unsourced (PAP 0), slot 1 sourced at level 60.
    assert_eq!(m.owners()[0], SlotOwner::NONE);
    assert_eq!(m.levels()[1], 60);

    // Activate the address, then withhold a coherent level+PAP update.
    sync(&mut rx, 120, cid(1), 100);
    assert!(merged(&dmx_sync(
        &mut rx,
        130,
        cid(1),
        1,
        2,
        100,
        &[70, 80],
        100,
        false
    ))
    .is_none());
    // The new PAP now sources slot 0 and drops slot 1.
    assert!(pap_sync(&mut rx, 131, cid(1), 1, 3, &[200, 0], 100)
        .iter()
        .all(|e| !matches!(e, ReceiverEvent::MergedData(_))));
    // Still frozen at the released frame.
    assert_eq!(rx.merged(uni(1)).unwrap().owners()[0], SlotOwner::NONE);

    // One sync latches the coherent level+PAP frame together.
    let e = sync(&mut rx, 140, cid(1), 100);
    let m = merged(&e).expect("coherent level+PAP frame released");
    assert_eq!(m.levels()[0], 70);
    assert_eq!(owner_cid(m, 0), Some(cid(1)));
    assert_eq!(m.owners()[1], SlotOwner::NONE);
}

#[test]
fn pap_lost_under_sync_is_withheld_until_release() {
    let mut rx = receiver(50);
    listen(&mut rx, 0, uni(1));
    // Source 1 sends PAP giving it a high per-slot priority; source 2 sends
    // only levels at universe priority.
    dmx_sync(&mut rx, 0, cid(1), 1, 0, 100, &[10], 100, false);
    pap_sync(&mut rx, 0, cid(1), 1, 1, &[200], 100);
    dmx_sync(&mut rx, 0, cid(2), 1, 0, 100, &[250], 100, false);
    let e = poll(&mut rx, 100);
    let m = merged(&e).expect("first frame");
    // Source 1's PAP (200) beats source 2's universe priority (100).
    assert_eq!(owner_cid(m, 0), Some(cid(1)));
    assert_eq!(m.levels()[0], 10);

    // Activate synchronization.
    sync(&mut rx, 120, cid(1), 100);
    // Keep both sources alive with withheld data.
    dmx_sync(&mut rx, 200, cid(1), 1, 2, 100, &[10], 100, false);
    dmx_sync(&mut rx, 200, cid(2), 1, 1, 100, &[250], 100, false);

    // Source 1's PAP times out (its last PAP was at t=0). A NULL packet after the
    // PAP timeout reveals the loss; the revert to universe priority is withheld.
    let e = dmx_sync(&mut rx, 2600, cid(1), 1, 3, 100, &[10], 100, false);
    assert!(e
        .iter()
        .any(|ev| matches!(ev, ReceiverEvent::SourcePapLost { .. })));
    // Output still frozen: source 1 still owns slot 0 in the published frame.
    assert_eq!(
        rx.merged(uni(1))
            .unwrap()
            .source(rx.merged(uni(1)).unwrap().owners()[0])
            .map(|s| s.cid),
        Some(cid(1))
    );

    // Refresh the sync so the address stays active, releasing the reverted frame.
    let e = sync(&mut rx, 2610, cid(1), 100);
    let m = merged(&e).expect("reverted frame released");
    // With source 1 back at universe priority (100 == source 2), HTP picks the
    // higher level.
    assert_eq!(m.levels()[0], 250);
    assert_eq!(owner_cid(m, 0), Some(cid(2)));
}
