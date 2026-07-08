//! Unit tests for the basic receiver.
//!
//! The tests uses a fixed capacity policy equal to the maximum number of
//! relevant resources that group reaches. This tests the boundary conditions
//! for the fixed-capacity storage at the same time as the actual test logic is
//! exercised.

use super::*;
use crate::packet::{DataPacket, Packet, Payload};
use crate::receiver::{BasicReceiverEvent, LostSource, SourceInfo, UniverseData};
use crate::static_storage;
use crate::time::Duration;
use crate::types::{Cid, NetintId, SequenceNumber};
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use alloc::vec;
use alloc::vec::Vec;

// --- test storage policies --------------------------------------------------

static_storage! {
    struct TestCaps {
        // Only one universe per test
        rx_universes: 1,
        // Grouped source-loss tests exercise two competing sources timing out
        // together
        rx_sources_per_universe: 2,
        rx_sync_addresses: 0,
        tx_universes: 0,
        det_sources: 0,
        det_universes_per_source: 0
    }
}

type BasicReceiver = super::BasicReceiver<TestCaps>;

impl BasicReceiver {
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

/// Builds and feeds a data packet in one step, returning the owned events the
/// receiver produced from it.
#[allow(clippy::too_many_arguments)]
fn feed(
    rx: &mut BasicReceiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
    start_code: u8,
    values: &[u8],
    preview: bool,
    terminated: bool,
) -> Vec<BasicReceiverEvent> {
    let packet = Packet {
        cid: source,
        payload: Payload::Data(DataPacket {
            source_name: "src",
            priority: 100,
            sync_address: 0,
            sequence_number: SequenceNumber::new(seq),
            preview,
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

fn dmx(
    rx: &mut BasicReceiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
) -> Vec<BasicReceiverEvent> {
    feed(
        rx,
        ms,
        source,
        universe,
        seq,
        DMX_NULL_START_CODE,
        &[1, 2, 3],
        false,
        false,
    )
}

fn pap(
    rx: &mut BasicReceiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
) -> Vec<BasicReceiverEvent> {
    feed(
        rx,
        ms,
        source,
        universe,
        seq,
        PAP_START_CODE,
        &[200, 200],
        false,
        false,
    )
}

fn terminate(
    rx: &mut BasicReceiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
) -> Vec<BasicReceiverEvent> {
    feed(
        rx,
        ms,
        source,
        universe,
        seq,
        DMX_NULL_START_CODE,
        &[],
        false,
        true,
    )
}

/// An alternate START code that is neither NULL (levels) nor `0xDD` (PAP).
const OTHER_START_CODE: u8 = 0x17;

fn other(
    rx: &mut BasicReceiver,
    ms: u64,
    source: Cid,
    universe: u16,
    seq: u8,
) -> Vec<BasicReceiverEvent> {
    feed(
        rx,
        ms,
        source,
        universe,
        seq,
        OTHER_START_CODE,
        &[7, 7, 7],
        false,
        false,
    )
}

/// Selects the `UniverseData` events from an event list.
fn data_events(events: &[BasicReceiverEvent]) -> Vec<&UniverseData> {
    events
        .iter()
        .filter_map(|e| match e {
            BasicReceiverEvent::UniverseData(d) => Some(d),
            _ => None,
        })
        .collect()
}

/// Returns the sources from the first `SourcesLost` event in an event list.
fn sources_lost(events: &[BasicReceiverEvent]) -> Option<&[LostSource]> {
    events.iter().find_map(|e| match e {
        BasicReceiverEvent::SourcesLost { sources, .. } => Some(sources.as_slice()),
        _ => None,
    })
}

/// Drains a `poll` into its deadline and its events in owned form.
fn poll_all(rx: &mut BasicReceiver, now: Instant) -> (Option<Instant>, Vec<BasicReceiverEvent>) {
    let mut outcome = rx.poll(now);
    let deadline = outcome.deadline;
    let mut events = Vec::new();
    while let Some(event) = outcome.next_event() {
        events.push(event.into());
    }
    (deadline, events)
}

/// [`poll_all`], keeping only the events.
fn poll_events(rx: &mut BasicReceiver, now: Instant) -> Vec<BasicReceiverEvent> {
    poll_all(rx, now).1
}

/// [`poll_all`], keeping only the deadline.
fn poll_deadline(rx: &mut BasicReceiver, now: Instant) -> Option<Instant> {
    poll_all(rx, now).0
}

/// Builds a receiver already listening to `universe`, discarding the opening
/// sampling-start notification (tests that care about it drive `listen`
/// directly).
fn listening(config: ReceiverConfig, universe: u16) -> BasicReceiver {
    let mut rx = BasicReceiver::new(config);
    rx.listen(instant(0), uni(universe))
        .expect("within the test's universe capacity");
    rx
}

// --- listen / stop_listening ------------------------------------------------

#[test]
fn listen_starts_sampling() {
    let mut rx = BasicReceiver::new(ReceiverConfig::default());
    let outcome = rx
        .listen(instant(0), uni(1))
        .expect("within the test's universe capacity");
    assert!(outcome.sampling_started);
}

#[test]
fn stop_listening_reports_whether_listening() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    let stopped = rx.stop_listening(uni(1));
    assert!(stopped.was_listening);
    // A second stop is a no-op.
    let stopped = rx.stop_listening(uni(1));
    assert!(!stopped.was_listening);
}

#[test]
fn relisten_does_not_restart_sampling() {
    let mut rx = listening(ReceiverConfig::default(), 1);
    // Re-listening a universe already listened to does not open a new sampling
    // period.
    let outcome = rx
        .listen(instant(100), uni(1))
        .expect("within the test's universe capacity");
    assert!(!outcome.sampling_started);
}

#[test]
fn listen_reports_a_full_universe_table() {
    let mut rx = BasicReceiver::new(ReceiverConfig::default());
    let cap = <TestCaps as BasicReceiverStorage>::BasicUniverses::CAPACITY as u16;
    for u in 1..cap + 1 {
        assert!(
            rx.listen(instant(0), uni(u))
                .expect("universe within capacity fits")
                .sampling_started
        );
    }
    assert_eq!(rx.listen(instant(0), uni(cap + 1)), Err(Error::NoCapacity));
    assert!(
        !rx.listen(instant(0), uni(cap))
            .expect("re-listening a tracked universe fits")
            .sampling_started
    );
}

// --- Sampling period ---------------------------------------------------------

#[test]
fn sampling_period_marks_data_and_ends_on_schedule() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    // poll right away: the only pending deadline is the end of the sampling
    // period at 1500ms.
    assert_eq!(poll_deadline(&mut rx, instant(0)), Some(instant(1500)));

    // A level-only source during sampling is reported immediately.
    let events = dmx(&mut rx, 100, cid(1), 1, 0);
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert!(data[0].is_sampling);
    assert_eq!(data[0].universe, uni(1));
    assert_eq!(data[0].source.cid, cid(1));

    // Ending the sampling period notifies once.
    assert_eq!(
        poll_events(&mut rx, instant(1500)),
        vec![BasicReceiverEvent::SamplingEnded { universe: uni(1) }]
    );

    // Subsequent data is no longer flagged as sampling.
    let events = dmx(&mut rx, 1600, cid(1), 1, 1);
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert!(!data[0].is_sampling);
}

#[test]
fn sample_period_is_configurable() {
    let config = ReceiverConfig::default().with_sample_period(Duration::from_millis(4000));
    let mut rx = listening(config, 1);

    // The pending deadline reflects the configured 4s period, not the default.
    assert_eq!(poll_deadline(&mut rx, instant(0)), Some(instant(4000)));

    // At the default 1.5s point the period is still in progress.
    assert!(poll_events(&mut rx, instant(1500)).is_empty());

    // It ends at the configured time.
    assert_eq!(
        poll_events(&mut rx, instant(4000)),
        vec![BasicReceiverEvent::SamplingEnded { universe: uni(1) }]
    );
}

#[test]
fn zero_sample_period_starts_and_ends_on_first_poll() {
    let config = ReceiverConfig::default().with_sample_period(Duration::ZERO);
    let mut rx = BasicReceiver::new(config);

    // The sampling period started at listen time.
    let outcome = rx
        .listen(instant(0), uni(1))
        .expect("within the test's universe capacity");
    assert!(outcome.sampling_started);

    // The first poll, even at the same instant, ends the zero-length period.
    assert_eq!(
        poll_events(&mut rx, instant(0)),
        vec![BasicReceiverEvent::SamplingEnded { universe: uni(1) }]
    );

    // Data after the period is no longer flagged as sampling. (A 0xDD-first
    // source reports right away outside a sampling period, unlike a level-only
    // one, which is held pending the PAP wait.)
    let events = pap(&mut rx, 0, cid(1), 1, 0);
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert!(!data[0].is_sampling);
}

#[test]
fn source_terminated_within_sampling_period_skips_term_logic() {
    // If a source is found, then sent with stream_terminated, within the sampling period, then the
    // termination set logic should be skipped and the source loss event should be pushed
    // immediately. A second, still-unknown source must not hold it up (which is what the grouping
    // would otherwise do).
    let mut rx = listening(ReceiverConfig::default(), 1);

    dmx(&mut rx, 0, cid(1), 1, 0);
    dmx(&mut rx, 0, cid(2), 1, 0);
    poll_events(&mut rx, instant(100)); // both marked online; still well within the 1.5s sampling period

    // cid(1) terminates; cid(2) goes silent (so it would classify as "unknown").
    terminate(&mut rx, 110, cid(1), 1, 1);

    let events = poll_events(&mut rx, instant(120));
    let lost = sources_lost(&events).expect("loss reported during sampling");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].cid, cid(1));
    assert!(lost[0].terminated);
}

#[test]
fn source_timed_out_within_sampling_period_skips_term_logic() {
    // If the sampling limit is configured such that it is longer than a network data loss timeout,
    // the behavior should be the same as terminated.
    let config = ReceiverConfig::default().with_sample_period(Duration::from_millis(4000));
    let mut rx = listening(config, 1);

    dmx(&mut rx, 0, cid(1), 1, 0); // times out at 2500
    dmx(&mut rx, 0, cid(2), 1, 0);
    dmx(&mut rx, 300, cid(2), 1, 1); // cid(2) times out later, at 2800
    poll_events(&mut rx, instant(1000)); // both marked online; still sampling (< 4000)

    let events = poll_events(&mut rx, instant(2500)); // cid(1) timed out; cid(2) still unknown
    let lost = sources_lost(&events).expect("loss reported during sampling");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].cid, cid(1));
    assert!(!lost[0].terminated);
}

#[test]
fn source_lost_within_sampling_period_with_extra_hold_time_skips_term_logic() {
    // Same as the above two tests but making sure that extra hold time is honored if it is set
    // shorter than the sampling period: the loss waits out the hold, but grouping is still skipped
    // (a still-unknown source does not extend the wait beyond the hold).
    let config = ReceiverConfig::default()
        .with_sample_period(Duration::from_millis(4000))
        .with_extra_hold_time(Duration::from_millis(500));
    let mut rx = listening(config, 1);

    dmx(&mut rx, 0, cid(1), 1, 0); // times out at 2500
    dmx(&mut rx, 0, cid(2), 1, 0);
    dmx(&mut rx, 600, cid(2), 1, 1); // cid(2) stays unknown past 3000 (times out at 3100)
    poll_events(&mut rx, instant(1000));

    // cid(1) times out, opening its hold window; nothing is reported yet.
    assert!(sources_lost(&poll_events(&mut rx, instant(2500))).is_none());

    // After the 500ms hold (still within the sampling period) it is reported, not held further by
    // the still-unknown cid(2).
    let events = poll_events(&mut rx, instant(3000));
    let lost = sources_lost(&events).expect("loss reported after the hold");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].cid, cid(1));
}

// --- Sequence numbering ------------------------------------------------------

#[test]
fn out_of_order_and_duplicate_packets_are_discarded() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    let events = [
        dmx(&mut rx, 0, cid(1), 1, 5), // accepted (new source)
        dmx(&mut rx, 1, cid(1), 1, 5), // duplicate -> discarded
        dmx(&mut rx, 2, cid(1), 1, 4), // older -> discarded
        dmx(&mut rx, 3, cid(1), 1, 6), // newer -> accepted
    ]
    .concat();
    assert_eq!(data_events(&events).len(), 2);

    // Jump well ahead, then a sequence number far behind: a difference of at
    // least 20 is treated as a wrap/restart and accepted, not a stale packet.
    let events = [
        dmx(&mut rx, 4, cid(1), 1, 200), // newer -> accepted
        dmx(&mut rx, 5, cid(1), 1, 1),   // 1 - 200 = -199 (<= -20) -> accepted as wrap
    ]
    .concat();
    assert_eq!(data_events(&events).len(), 2);
}

// --- Per-address priority ----------------------------------------------------

#[test]
fn new_source_outside_sampling_waits_for_pap_then_reports() {
    let mut rx = listening(ReceiverConfig::default(), 1);
    poll_events(&mut rx, instant(1500)); // end the sampling period

    // First levels packet outside sampling: withheld pending a 0xDD packet.
    assert!(data_events(&dmx(&mut rx, 2000, cid(1), 1, 0)).is_empty());

    // Still within the 1.5s PAP wait (started at 2000): still withheld.
    assert!(data_events(&dmx(&mut rx, 3499, cid(1), 1, 1)).is_empty());

    // The PAP wait has now elapsed: fall back to packet priority and report.
    assert_eq!(data_events(&dmx(&mut rx, 3500, cid(1), 1, 2)).len(), 1);
}

#[test]
fn new_source_reports_dmx_after_receiving_pap_within_pap_wait_time() {
    let mut rx = listening(ReceiverConfig::default(), 1);
    poll_events(&mut rx, instant(1500)); // end the sampling period

    // First levels packet outside sampling: withheld pending a 0xDD packet.
    assert!(data_events(&dmx(&mut rx, 2000, cid(1), 1, 0)).is_empty());

    // Then a PAP is received still within the PAP wait time.
    let events = pap(&mut rx, 2100, cid(1), 1, 1);
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].start_code, PAP_START_CODE);
    assert!(!data[0].is_sampling);

    // Now a DMX packet (still within the PAP wait time) should be passed.
    let events = dmx(&mut rx, 2200, cid(1), 1, 2);
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].start_code, DMX_NULL_START_CODE);
    assert!(!data[0].is_sampling);
}

#[test]
fn pap_first_source_reports_immediately() {
    let mut rx = listening(ReceiverConfig::default(), 1);
    poll_events(&mut rx, instant(1500));

    let events = pap(&mut rx, 2000, cid(1), 1, 0);
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].start_code, PAP_START_CODE);
    assert!(!data[0].is_sampling);
}

#[test]
fn pap_then_dmx_both_reported() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    let events = [
        pap(&mut rx, 0, cid(1), 1, 0),
        dmx(&mut rx, 10, cid(1), 1, 1),
    ]
    .concat();
    let data = data_events(&events);
    assert_eq!(data.len(), 2);
    assert_eq!(data[0].start_code, PAP_START_CODE);
    assert_eq!(data[1].start_code, DMX_NULL_START_CODE);
}

#[test]
fn pap_lost_when_priority_stops_but_levels_continue() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    // Establish a source sending both PAP and levels (during sampling).
    pap(&mut rx, 0, cid(1), 1, 0);
    dmx(&mut rx, 10, cid(1), 1, 1);

    // PAP stops; a levels packet after the PAP timeout (2.5s) triggers a
    // PAP-lost notification, delivered before the data itself.
    let events = dmx(&mut rx, 2600, cid(1), 1, 2);
    assert_eq!(
        events[0],
        BasicReceiverEvent::SourcePapLost {
            universe: uni(1),
            source: SourceInfo {
                cid: cid(1),
                name: "src".into()
            }
        }
    );
    assert_eq!(data_events(&events).len(), 1);
}

#[test]
fn pap_wait_time_is_configurable() {
    let config =
        ReceiverConfig::default().with_per_address_priority_wait_time(Duration::from_millis(800));
    let mut rx = listening(config, 1);
    poll_events(&mut rx, instant(1500)); // end the sampling period

    // First levels packet outside sampling: withheld pending a 0xDD packet.
    assert!(data_events(&dmx(&mut rx, 2000, cid(1), 1, 0)).is_empty());

    // Still within the configured 800ms wait (started at 2000): still withheld.
    assert!(data_events(&dmx(&mut rx, 2799, cid(1), 1, 1)).is_empty());

    // The configured wait has elapsed (not the 1.5s default): now reported.
    assert_eq!(data_events(&dmx(&mut rx, 2800, cid(1), 1, 2)).len(), 1);
}

#[test]
fn pap_handling_disabled_reports_levels_immediately() {
    let config = ReceiverConfig::default().with_per_address_priority_handling(false);
    let mut rx = listening(config, 1);
    poll_events(&mut rx, instant(1500)); // end the sampling period

    // With handling disabled, a new level-only source outside sampling is not
    // held back to wait for a 0xDD packet: its first packet is reported at once.
    let events = dmx(&mut rx, 2000, cid(1), 1, 0);
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].start_code, DMX_NULL_START_CODE);
}

#[test]
fn pap_handling_disabled_still_forwards_pap_and_never_loses_it() {
    let config = ReceiverConfig::default().with_per_address_priority_handling(false);
    let mut rx = listening(config, 1);

    // 0xDD packets are still delivered (only the handling logic is disabled).
    let events = pap(&mut rx, 0, cid(1), 1, 0);
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].start_code, PAP_START_CODE);

    dmx(&mut rx, 10, cid(1), 1, 1);

    // The scenario that would produce a PAP-lost with handling enabled (PAP
    // stops, levels continue past the PAP timeout) produces no such event here,
    // and the levels are still forwarded.
    let events = dmx(&mut rx, 2600, cid(1), 1, 2);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, BasicReceiverEvent::SourcePapLost { .. }))
    );
    assert_eq!(data_events(&events).len(), 1);
}

#[test]
fn pap_wait_does_not_apply_during_sampling() {
    // Interaction of two knobs: even with a very long PAP wait, a level-only
    // source discovered during the sampling period is reported immediately - the
    // PAP wait governs only sources found after sampling ends.
    let config =
        ReceiverConfig::default().with_per_address_priority_wait_time(Duration::from_millis(5000));
    let mut rx = listening(config, 1);

    assert_eq!(data_events(&dmx(&mut rx, 100, cid(1), 1, 0)).len(), 1);
}

#[test]
fn pap_wait_is_bounded_by_the_loss_timeout() {
    // Interaction of two knobs: the PAP wait can be set longer than the (fixed)
    // 2.5s source loss timeout, but a silent source is still governed by the
    // loss timer. A source that sends one packet and stops is dropped at the
    // loss timeout - never reported - even though its PAP wait had not elapsed.
    let config =
        ReceiverConfig::default().with_per_address_priority_wait_time(Duration::from_millis(4000));
    let mut rx = listening(config, 1);
    poll_events(&mut rx, instant(1500)); // end the sampling period

    // A single levels packet at 2000, then silence: withheld pending PAP.
    assert!(data_events(&dmx(&mut rx, 2000, cid(1), 1, 0)).is_empty());

    // The pending deadline is the loss timeout (2000 + 2500), not the PAP wait
    // (which would be 2000 + 4000 = 6000).
    let (deadline, events) = poll_all(&mut rx, instant(2000));
    assert_eq!(deadline, Some(instant(4500)));
    assert!(events.is_empty());

    // At the loss timeout the silent source is dropped, with no event of any
    // kind, and nothing is left pending.
    let (deadline, events) = poll_all(&mut rx, instant(4500));
    assert_eq!(deadline, None);
    assert!(events.is_empty());
}

// --- Network data loss / START codes -----------------------------------------

#[test]
fn any_start_code_resets_the_loss_timer() {
    let config = ReceiverConfig::default().with_allowed_start_codes(&[
        DMX_NULL_START_CODE,
        PAP_START_CODE,
        OTHER_START_CODE,
    ]);
    let mut rx = listening(config, 1);

    dmx(&mut rx, 0, cid(1), 1, 0); // tracked during sampling; NULL timer -> 2500
    poll_events(&mut rx, instant(1500)); // end sampling

    // A PAP packet before the NULL timeout extends the loss timer to 2000+2500.
    pap(&mut rx, 2000, cid(1), 1, 1);
    assert!(
        sources_lost(&poll_events(&mut rx, instant(2600))).is_none(), // past the original 2500 timeout
        "PAP should have kept the source alive past the NULL timeout"
    );

    // An unrelated alternate START code does the same, extending it to 4000+2500.
    let events = other(&mut rx, 4000, cid(1), 1, 2);
    assert_eq!(data_events(&events).len(), 1); // the 0x17 data is forwarded
    assert_eq!(data_events(&events)[0].start_code, OTHER_START_CODE);
    assert!(sources_lost(&poll_events(&mut rx, instant(4600))).is_none());

    // Once truly silent, it is lost at the loss timeout after the last packet.
    let events = poll_events(&mut rx, instant(6500));
    let lost = sources_lost(&events).expect("source lost once silent");
    assert_eq!(lost[0].cid, cid(1));
}

#[test]
fn source_sending_only_unlisted_start_code_is_ignored() {
    // By default only NULL and PAP are processed. A source that only ever sends
    // some other (non-allow-listed) START code is ignored entirely: its data is
    // not forwarded, it is not tracked, and it does not count toward the limit.
    let mut rx = listening(ReceiverConfig::default(), 1);

    assert!(other(&mut rx, 0, cid(1), 1, 0).is_empty());
    assert!(other(&mut rx, 100, cid(1), 1, 1).is_empty());
    // Nothing is tracked, so once sampling ends there is no pending deadline.
    assert_eq!(poll_deadline(&mut rx, instant(1500)), None);
}

#[test]
fn allow_listed_start_code_tracks_forwards_and_loses_source() {
    // With only an alternate START code allow-listed (NULL and PAP excluded), a
    // source seen sending only that code is tracked and forwarded like any
    // other, and its loss is reported.
    let config = ReceiverConfig::default().with_allowed_start_codes(&[OTHER_START_CODE]);
    let mut rx = listening(config, 1);

    let events = other(&mut rx, 0, cid(1), 1, 0);
    assert_eq!(data_events(&events).len(), 1);
    assert_eq!(data_events(&events)[0].start_code, OTHER_START_CODE);

    poll_events(&mut rx, instant(1500)); // end sampling

    // It goes silent and is reported lost at the loss timeout (not dropped
    // silently - it was delivered).
    let events = poll_events(&mut rx, instant(2500));
    let lost = sources_lost(&events).expect("an allow-listed source is reported lost");
    assert_eq!(lost[0].cid, cid(1));
}

#[test]
fn allow_list_can_exclude_null() {
    // The allow-list is authoritative: excluding NULL means level data is ignored
    // entirely, while a still-allowed code (here PAP) is processed normally.
    let config = ReceiverConfig::default().with_allowed_start_codes(&[PAP_START_CODE]);
    let mut rx = listening(config, 1);

    // NULL is not processed: ignored, no source tracked.
    assert!(dmx(&mut rx, 0, cid(1), 1, 0).is_empty());
    assert_eq!(poll_deadline(&mut rx, instant(0)), Some(instant(1500))); // only the sampling deadline

    // PAP is still processed.
    let events = pap(&mut rx, 10, cid(2), 1, 0);
    assert_eq!(data_events(&events).len(), 1);
    assert_eq!(data_events(&events)[0].source.cid, cid(2));
    assert_eq!(data_events(&events)[0].start_code, PAP_START_CODE);
}

#[test]
fn allow_list_can_exclude_pap() {
    // Excluding PAP means 0xDD packets are ignored entirely (distinct from
    // disabling PAP handling, which still forwards them).
    let config = ReceiverConfig::default().with_allowed_start_codes(&[DMX_NULL_START_CODE]);
    let mut rx = listening(config, 1);

    // PAP is not processed: ignored, no source tracked.
    assert!(pap(&mut rx, 0, cid(1), 1, 0).is_empty());

    // NULL is still processed (reported immediately during sampling).
    let events = dmx(&mut rx, 10, cid(2), 1, 0);
    assert_eq!(data_events(&events).len(), 1);
    assert_eq!(data_events(&events)[0].source.cid, cid(2));
    assert_eq!(data_events(&events)[0].start_code, DMX_NULL_START_CODE);
}

#[test]
fn excluding_pap_implicitly_disables_pap_handling() {
    // PAP handling defaults to on, but with 0xDD excluded from the allow-list
    // there is no PAP to wait for, so a new source's first NULL data outside a
    // sampling period is delivered immediately instead of being withheld for the
    // PAP wait. (Contrast `new_source_outside_sampling_waits_for_pap_then_reports`,
    // where PAP is allow-listed and the levels are withheld.)
    let config = ReceiverConfig::default().with_allowed_start_codes(&[DMX_NULL_START_CODE]);
    let mut rx = listening(config, 1);
    poll_events(&mut rx, instant(1500)); // end sampling, so the PAP wait would otherwise apply

    let events = dmx(&mut rx, 2000, cid(1), 1, 0);
    assert_eq!(data_events(&events).len(), 1);
    assert_eq!(data_events(&events)[0].start_code, DMX_NULL_START_CODE);
}

#[test]
fn delivered_source_entering_pap_wait_is_reported_lost_not_dropped() {
    // A source delivered via an allow-listed alternate START code, then sending
    // its first NULL (which enters the PAP wait and withholds the levels), must
    // still be reported lost when it times out - it is not silently dropped,
    // because it has already been delivered.
    let config = ReceiverConfig::default()
        .with_allowed_start_codes(&[DMX_NULL_START_CODE, PAP_START_CODE, OTHER_START_CODE])
        .with_per_address_priority_wait_time(Duration::from_millis(3000));
    let mut rx = listening(config, 1);
    poll_events(&mut rx, instant(1500)); // end sampling so the NULL is subject to the PAP wait

    // Delivered via the alternate START code.
    assert_eq!(data_events(&other(&mut rx, 2000, cid(1), 1, 0)).len(), 1);

    // First NULL with no prior PAP: enters WaitingForPap.
    assert!(data_events(&dmx(&mut rx, 2100, cid(1), 1, 1)).is_empty());

    // It then goes silent and times out (last packet at 2100 -> 4600). Because
    // it was already delivered, this is a reported loss, not a silent drop.
    let events = poll_events(&mut rx, instant(4600));
    let lost = sources_lost(&events).expect("a delivered source's loss is reported");
    assert_eq!(lost[0].cid, cid(1));
}

// --- Source loss + settling --------------------------------------------------

#[test]
fn near_simultaneous_losses_are_grouped() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    // Two sources established during the sampling period.
    dmx(&mut rx, 0, cid(1), 1, 0);
    dmx(&mut rx, 0, cid(2), 1, 0);
    // Stagger their last-seen times so they time out a little apart.
    dmx(&mut rx, 100, cid(1), 1, 1); // source 1 expires at 2600ms
    dmx(&mut rx, 300, cid(2), 1, 1); // source 2 expires at 2800ms
    poll_events(&mut rx, instant(1500)); // end sampling; both still online

    // Source 1 has timed out; source 2 is still within its window. A
    // termination set opens, capturing source 2 as not-yet-resolved, so nothing
    // is reported yet even though there is no extra hold time configured.
    let (deadline, events) = poll_all(&mut rx, instant(2650));
    assert!(deadline.is_some());
    assert!(events.is_empty());

    // Source 2 now times out too: with both confirmed offline the set settles
    // and fires immediately (the default extra hold time is zero), reporting
    // them together in a single notification.
    let events = poll_events(&mut rx, instant(2850));
    assert_eq!(events.len(), 1);
    match &events[0] {
        BasicReceiverEvent::SourcesLost { universe, sources } => {
            assert_eq!(*universe, uni(1));
            assert_eq!(sources.len(), 2);
            assert_eq!(sources[0].cid, cid(1));
            assert_eq!(sources[1].cid, cid(2));
            assert!(sources.iter().all(|s| !s.terminated));
        }
        other => panic!("expected SourcesLost, got {other:?}"),
    }
}

#[test]
fn unknown_source_online_resolves_grouped_loss() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    // Two sources established during the sampling period.
    dmx(&mut rx, 0, cid(1), 1, 0);
    dmx(&mut rx, 0, cid(2), 1, 0);
    // Stagger their last-seen times so they time out a little apart.
    dmx(&mut rx, 100, cid(1), 1, 1); // source 1 expires at 2600ms
    dmx(&mut rx, 300, cid(2), 1, 1); // source 2 expires at 2800ms
    poll_events(&mut rx, instant(1500)); // end sampling; both still online

    // Source 1 has timed out; source 2 is still within its window. A
    // termination set opens, capturing source 2 as not-yet-resolved.
    let (deadline, events) = poll_all(&mut rx, instant(2650));
    assert!(deadline.is_some());
    assert!(events.is_empty());

    // Source 2 proves itself online by sending valid DMX.
    dmx(&mut rx, 2700, cid(2), 1, 2);

    // Source 1 is reported as offline now that all other sources' states are resolved.
    let events = poll_events(&mut rx, instant(2710));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, BasicReceiverEvent::SourcesLost {
        sources, ..
    } if sources.len() == 1 && sources[0].cid == cid(1)))
    );
}

#[test]
fn lone_loss_with_no_extra_hold_is_reported_as_soon_as_detected() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    dmx(&mut rx, 0, cid(1), 1, 0); // established during sampling; expires at 2500
    poll_events(&mut rx, instant(1500));

    // With the default zero extra hold time and no other sources to settle
    // against, the loss is reported right away at 2.5 seconds.
    let events = poll_events(&mut rx, instant(2500));
    assert!(matches!(
        &events[0],
        BasicReceiverEvent::SourcesLost { sources, .. } if sources.len() == 1 && sources[0].cid == cid(1)
    ));
}

#[test]
fn extra_hold_time_delays_loss_and_lets_a_returning_source_cancel_it() {
    // A configured extra hold time opens a window in which a returning source
    // can cancel its own pending loss.
    let config = ReceiverConfig::default().with_extra_hold_time(Duration::from_millis(500));
    let mut rx = listening(config, 1);

    dmx(&mut rx, 0, cid(1), 1, 0); // expires at 2500
    poll_events(&mut rx, instant(1500));

    // Just after the loss timeout: a set opens but the hold has not elapsed.
    assert!(poll_events(&mut rx, instant(2500)).is_empty());

    // The source comes back within the hold window. The returning packet is
    // forwarded, and the following poll reports no loss.
    let mut events = dmx(&mut rx, 2600, cid(1), 1, 1);
    events.extend(poll_events(&mut rx, instant(2700)));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, BasicReceiverEvent::SourcesLost { .. }))
    );
    assert_eq!(data_events(&events).len(), 1); // the returning packet was forwarded
}

// --- Termination -------------------------------------------------------------

#[test]
fn terminated_stream_is_reported_as_loss() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    dmx(&mut rx, 0, cid(1), 1, 0);

    // A terminated packet is not itself forwarded.
    assert!(data_events(&terminate(&mut rx, 10, cid(1), 1, 1)).is_empty());

    // Termination expires the source's timer immediately, so (with the default
    // zero extra hold time) it is reported lost, marked terminated, on the next
    // poll.
    let events = poll_events(&mut rx, instant(10));
    let lost = sources_lost(&events).expect("expected a SourcesLost notification");
    assert_eq!(lost.len(), 1);
    assert!(lost[0].terminated);
}

#[test]
fn packets_from_terminated_source_are_ignored() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    dmx(&mut rx, 0, cid(1), 1, 0);
    terminate(&mut rx, 10, cid(1), 1, 1);

    // Further data from the same source (pre-removal) is dropped.
    assert!(data_events(&dmx(&mut rx, 20, cid(1), 1, 2)).is_empty());
}

// --- Source limit ------------------------------------------------------------

#[test]
fn source_limit_exceeded_is_emitted_once() {
    let config = ReceiverConfig::default().with_source_limit(1);
    let mut rx = listening(config, 1);

    let events = [
        dmx(&mut rx, 0, cid(1), 1, 0), // tracked
        dmx(&mut rx, 0, cid(2), 1, 0), // over the limit
        dmx(&mut rx, 0, cid(3), 1, 0), // still over the limit, suppressed
    ]
    .concat();
    let exceeded = events
        .iter()
        .filter(|e| matches!(e, BasicReceiverEvent::SourceLimitExceeded { .. }))
        .count();
    assert_eq!(exceeded, 1);
    // Only the one tracked source produced data.
    assert_eq!(data_events(&events).len(), 1);
}

// --- Filtering and routing ---------------------------------------------------

#[test]
fn preview_data_is_filtered_when_configured() {
    let config = ReceiverConfig::default().with_filter_preview(true);
    let mut rx = listening(config, 1);

    let events = feed(
        &mut rx,
        0,
        cid(1),
        1,
        0,
        DMX_NULL_START_CODE,
        &[1, 2, 3],
        true,
        false,
    );
    assert!(data_events(&events).is_empty());
}

#[test]
fn data_for_unlistened_universe_is_ignored() {
    let mut rx = listening(ReceiverConfig::default(), 1);

    // universe 2 is not listened to
    assert!(dmx(&mut rx, 0, cid(1), 2, 0).is_empty());
}

fn sync_packet(
    rx: &mut BasicReceiver,
    ms: u64,
    source: Cid,
    sync_address: u16,
) -> Vec<BasicReceiverEvent> {
    use crate::packet::SyncPacket;
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

#[test]
fn sync_packet_is_surfaced_as_an_event() {
    let mut rx = listening(ReceiverConfig::default(), 1);
    let events = sync_packet(&mut rx, 0, cid(1), 100);
    assert_eq!(
        events,
        vec![BasicReceiverEvent::SyncReceived {
            sync_address: 100,
            cid: cid(1),
        }]
    );
}

#[test]
fn sync_packet_is_dropped_when_synchronization_disabled() {
    let mut rx = listening(ReceiverConfig::default().with_synchronization(false), 1);
    assert!(sync_packet(&mut rx, 0, cid(1), 100).is_empty());
}

#[test]
fn data_surfaces_its_sync_address() {
    let mut rx = listening(
        ReceiverConfig::default().with_per_address_priority_handling(false),
        1,
    );
    let packet = Packet {
        cid: cid(1),
        payload: Payload::Data(DataPacket {
            source_name: "src",
            priority: 100,
            sync_address: 100,
            sequence_number: SequenceNumber::new(0),
            preview: false,
            stream_terminated: false,
            force_sync: false,
            universe: 1,
            start_code: DMX_NULL_START_CODE,
            values: &[1, 2, 3],
        }),
    };
    let mut events = Vec::new();
    rx.handle_packet(instant(0), test_addr(), NetintId::UNKNOWN, &packet)
        .for_each_owned(|e| events.push(e));
    let data = data_events(&events);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].sync_address, 100);
}

#[test]
fn sync_address_is_zeroed_when_synchronization_disabled() {
    let mut rx = listening(
        ReceiverConfig::default()
            .with_per_address_priority_handling(false)
            .with_synchronization(false),
        1,
    );
    let packet = Packet {
        cid: cid(1),
        payload: Payload::Data(DataPacket {
            source_name: "src",
            priority: 100,
            sync_address: 100,
            sequence_number: SequenceNumber::new(0),
            preview: false,
            stream_terminated: false,
            force_sync: false,
            universe: 1,
            start_code: DMX_NULL_START_CODE,
            values: &[1, 2, 3],
        }),
    };
    let mut events = Vec::new();
    rx.handle_packet(instant(0), test_addr(), NetintId::UNKNOWN, &packet)
        .for_each_owned(|e| events.push(e));
    assert_eq!(data_events(&events)[0].sync_address, 0);
}
