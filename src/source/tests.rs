//! Unit tests for the source.
//!
//! The suite runs against fixed-capacity storage policies, so the
//! allocation-free `heapless::Vec` and [`SortedVecMap`](crate::SortedVecMap)
//! code paths get exactly the same coverage as the source logic. The heap
//! policy is exercised end-to-end by the adapter tests.

use alloc::vec::Vec;

use super::*;
use crate::packet::{Packet, Payload};
use crate::static_storage;
use crate::time::Duration;
use crate::types::Cid;

// --- test storage policies ---------------------------------------------------

static_storage! {
    struct TestCaps {
        rx_universes: 0,
        rx_sources_per_universe: 0,
        rx_sync_addresses: 0,
        tx_universes: 3,
        det_sources: 0,
        det_universes_per_source: 0
    }
}

type Source = super::Source<TestCaps>;

impl Source {
    fn new(config: SourceConfig) -> Self {
        Self::with_config(config)
    }
}

// A policy sized for a source that announces a full discovery page and one
// more universe, so its discovery run spans two pages.
static_storage! {
    struct BigCaps {
        rx_universes: 0,
        rx_sources_per_universe: 0,
        rx_sync_addresses: 0,
        tx_universes: MAX_UNIVERSES_PER_PAGE + 1,
        det_sources: 0,
        det_universes_per_source: 0
    }
}

fn at(ms: u64) -> Instant {
    Instant::from_epoch(Duration::from_millis(ms))
}

fn test_source() -> Source {
    Source::new(
        SourceConfig::new(Cid::from_bytes([1; 16]), "test source").with_sync_delay(Duration::ZERO),
    )
}

fn univ(n: u16) -> Universe {
    Universe::new(n).unwrap()
}

/// One drained transmission, in owned form. Each transmission borrows a buffer
/// reused for the next one, so tests copy the bytes out before pulling more.
#[derive(Clone, Debug)]
struct OwnedTx {
    route: Route,
    bytes: Vec<u8>,
}

/// Polls at `now`, draining every queued transmission into owned form and
/// returning the poll deadline alongside.
fn poll_at<S: super::SourceStorage>(
    source: &mut super::Source<S>,
    now: Instant,
) -> (Option<Instant>, Vec<OwnedTx>) {
    let mut poll = source.poll(now);
    let deadline = poll.deadline;
    let mut txs = Vec::new();
    while let Some(t) = poll.next_transmission() {
        txs.push(OwnedTx {
            route: t.route,
            bytes: t.data.to_vec(),
        });
    }
    (deadline, txs)
}

/// Polls at `now`, draining every queued transmission, and returns the universes
/// the poll reported as physically removed (see [`SourcePoll::removed`]).
fn removed_at<S: super::SourceStorage>(
    source: &mut super::Source<S>,
    now: Instant,
) -> Vec<Universe> {
    let mut poll = source.poll(now);
    while poll.next_transmission().is_some() {}
    poll.removed().to_vec()
}

/// The parsed data packets among `txs` routed to a universe.
fn universe_data_packets(txs: &[OwnedTx]) -> Vec<DataPacket<'_>> {
    let mut packets = Vec::new();
    for t in txs {
        if matches!(t.route, Route::Universe(_)) {
            if let Payload::Data(data) = Packet::parse(&t.bytes).unwrap().payload {
                packets.push(data);
            }
        }
    }
    packets
}

#[derive(Clone, Debug)]
struct OwnedData {
    start_code: u8,
    sequence: u8,
    terminated: bool,
    values: Vec<u8>,
    route: Route,
}

/// Every data transmission among `txs` in an owned, inspectable form (discovery
/// packets are skipped).
fn data_packets(txs: &[OwnedTx]) -> Vec<OwnedData> {
    let mut out = Vec::new();
    for t in txs {
        let packet = Packet::parse(&t.bytes).unwrap();
        if let Payload::Data(d) = packet.payload {
            out.push(OwnedData {
                start_code: d.start_code,
                sequence: d.sequence_number.get(),
                terminated: d.stream_terminated,
                values: d.values.to_vec(),
                route: t.route,
            });
        }
    }
    out
}

#[test]
fn no_universes_emits_nothing() {
    let mut source = test_source();
    let (deadline, txs) = poll_at(&mut source, at(0));
    assert_eq!(txs.len(), 0);
    assert_eq!(deadline, None);
}

#[test]
fn universe_without_levels_emits_nothing() {
    let mut source = test_source();
    assert!(source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity"));
    let (deadline, txs) = poll_at(&mut source, at(0));
    assert_eq!(txs.len(), 0);
    assert_eq!(deadline, None);
}

#[test]
fn levels_are_sent_with_correct_fields() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(7)))
        .expect("should have capacity");
    source.update_levels(univ(7), &[10, 20, 30]);

    let (_deadline, txs) = poll_at(&mut source, at(0));
    let packets = universe_data_packets(&txs);
    assert_eq!(packets.len(), 1);
    let data = packets[0];
    assert_eq!(data.universe, 7);
    assert_eq!(data.start_code, DMX_NULL_START_CODE);
    assert_eq!(data.priority, Priority::DEFAULT.get());
    assert_eq!(data.values, &[10, 20, 30]);
    assert_eq!(data.sequence_number.get(), 0);
    assert!(!data.stream_terminated);
}

#[test]
fn pre_suppression_burst_then_keepalive() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[1, 2, 3]);

    // Three rapid packets at the tick rate; the third transitions to keep-alive.
    let mut now = 0u64;
    for expected_seq in 0..PRE_SUPPRESSION_PACKETS {
        let (deadline, txs) = poll_at(&mut source, at(now));
        let packets = data_packets(&txs);
        assert_eq!(packets.len(), 1, "one multicast packet per tick");
        assert_eq!(packets[0].sequence, expected_seq);
        let is_last = expected_seq == PRE_SUPPRESSION_PACKETS - 1;
        let expected_gap = if is_last { 900 } else { 22 };
        assert_eq!(deadline, Some(at(now + expected_gap)));
        now += 22;
    }

    // The previous (fourth) send was at t=44, so the keep-alive is due at 844.
    let (deadline, txs) = poll_at(&mut source, at(now));
    assert_eq!(
        data_packets(&txs).len(),
        0,
        "nothing due before the keep-alive"
    );
    assert_eq!(deadline, Some(at(44 + 900)));

    // At the keep-alive deadline a single packet goes out again.
    let (_deadline, txs) = poll_at(&mut source, at(44 + 900));
    assert_eq!(data_packets(&txs).len(), 1);
}

#[test]
fn changing_levels_resets_suppression() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);

    // Burn through the burst into suppression (last send at t=44 -> due at 844).
    let mut now = 0u64;
    for _ in 0..PRE_SUPPRESSION_PACKETS {
        poll_at(&mut source, at(now));
        now += 22;
    }
    let (deadline, txs) = poll_at(&mut source, at(now));
    assert_eq!(data_packets(&txs).len(), 0);
    assert_eq!(deadline, Some(at(44 + 900)));

    // A change restarts the rapid burst.
    source.update_levels(univ(1), &[2]);
    let (deadline, txs) = poll_at(&mut source, at(now + 10));
    assert_eq!(data_packets(&txs)[0].values, &[2]);
    assert_eq!(deadline, Some(at(now + 10 + 22)));
}

#[test]
fn repeated_identical_levels_do_not_defeat_suppression() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");

    // Push the same frame on every tick across the whole pre-suppression burst.
    let mut now = 0u64;
    for _ in 0..PRE_SUPPRESSION_PACKETS {
        source.update_levels(univ(1), &[7, 7, 7]);
        poll_at(&mut source, at(now));
        now += 22;
    }

    // Pushing the same data again must not schedule an immediate re-send: the
    // next packet is the keep-alive, 900 ms after the last burst send (t=44).
    source.update_levels(univ(1), &[7, 7, 7]);
    let (deadline, txs) = poll_at(&mut source, at(now));
    assert_eq!(
        data_packets(&txs).len(),
        0,
        "identical data must not restart the burst"
    );
    assert_eq!(deadline, Some(at(44 + 900)));
}

#[test]
fn rapid_changes_never_exceed_the_dmx_rate() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[0]);

    // Drive the source the way an adapter does: poll, then either advance to the
    // returned deadline or apply a data update on a faster (30 ms) cadence,
    // whichever comes first. Record every instant a packet actually goes out.
    let mut sends = Vec::new();
    let mut now = 0u64;
    let mut next_frame = 30u64;
    let mut value = 0u8;
    while now <= 500 {
        let (deadline, txs) = poll_at(&mut source, at(now));
        if !data_packets(&txs).is_empty() {
            sends.push(now);
        }
        let deadline = deadline.expect("an active universe always has a deadline");
        let deadline_ms = deadline.since_epoch().as_millis() as u64;
        if next_frame <= deadline_ms {
            // The 30 ms frame update fires first: change the data and advance to it.
            now = next_frame;
            next_frame += 30;
            value = value.wrapping_add(1);
            source.update_levels(univ(1), &[value]);
        } else {
            now = deadline_ms;
        }
    }

    // Consecutive sends must be at least one tick interval apart.
    for pair in sends.windows(2) {
        assert!(
            pair[1] - pair[0] >= 22,
            "sends {} and {} are only {} ms apart, exceeding the DMX rate",
            pair[0],
            pair[1],
            pair[1] - pair[0],
        );
    }
}

#[test]
fn pap_sends_as_expected() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels_and_pap(univ(1), &[100, 100, 100], &[200, 0, 50]);

    let (_deadline, txs) = poll_at(&mut source, at(0));
    let packets = data_packets(&txs);
    let levels: Vec<_> = packets
        .iter()
        .filter(|p| p.start_code == DMX_NULL_START_CODE)
        .collect();
    let pap: Vec<_> = packets
        .iter()
        .filter(|p| p.start_code == PAP_START_CODE)
        .collect();
    assert_eq!(levels.len(), 1);
    assert_eq!(pap.len(), 1);
    assert_eq!(levels[0].values, &[100, 100, 100]);
    assert_eq!(pap[0].values, &[200, 0, 50]);
    // PAP is sent after levels and carries the next sequence number.
    assert_eq!(levels[0].sequence, 0);
    assert_eq!(pap[0].sequence, 1);
}

#[test]
fn termination_sends_three_packets_then_drops() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[5, 6]);
    poll_at(&mut source, at(0)); // get it transmitting

    source.remove_universe(univ(1));
    let mut now = 100u64;
    for i in 0..TERMINATION_PACKETS {
        // The universe is still present right up to (and during) the last packet.
        assert!(source.has_universe(univ(1)));
        let (deadline, txs) = poll_at(&mut source, at(now));
        let packets = data_packets(&txs);
        assert_eq!(packets.len(), 1, "termination packet {i} sent");
        assert!(packets[0].terminated, "termination packet sets the flag");
        assert_eq!(packets[0].values, &[5, 6], "carries the last levels");
        if i + 1 < TERMINATION_PACKETS {
            // More to send: the next packet is one tick away.
            assert_eq!(deadline, Some(at(now + 22)));
        } else {
            // The last packet completes termination. Nothing remains to schedule,
            // so the deadline must be None - not a far-future discovery
            // announcement that would stall a drain loop sleeping until it.
            assert_eq!(
                deadline, None,
                "no deadline after the final termination packet"
            );
        }
        now += 23;
    }

    // The third termination packet's poll also drops the universe.
    assert!(!source.has_universe(univ(1)));
    let (deadline, txs) = poll_at(&mut source, at(now));
    assert_eq!(data_packets(&txs).len(), 0);
    assert_eq!(deadline, None);
}

#[test]
fn poll_reports_removed_universes() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[5, 6]);

    // A poll that drops nothing reports an empty slice.
    assert!(removed_at(&mut source, at(0)).is_empty());

    source.remove_universe(univ(1));
    let mut now = 100u64;

    // While terminating, the universe is still tracked, so it is not reported -
    // not even on the final packet's poll, which only marks it finished. The core
    // physically drops it (and reports it) on the following poll.
    for _ in 0..TERMINATION_PACKETS {
        assert!(
            removed_at(&mut source, at(now)).is_empty(),
            "not reported while still terminating"
        );
        now += 23;
    }

    // The next poll physically drops the finished universe and reports it once.
    assert_eq!(removed_at(&mut source, at(now)), alloc::vec![univ(1)]);
    now += 23;

    // The report is rebuilt each poll, so it is empty again afterwards.
    assert!(removed_at(&mut source, at(now)).is_empty());
}

#[test]
fn poll_reports_all_universes_removed_together() {
    let mut source = test_source();
    for u in [univ(3), univ(1), univ(2)] {
        source
            .add_universe(UniverseConfig::new(u))
            .expect("should have capacity");
        source.update_levels(u, &[0]);
    }
    poll_at(&mut source, at(0)); // get them transmitting

    // Terminate all three at once.
    for u in [univ(1), univ(2), univ(3)] {
        source.remove_universe(u);
    }

    // Drive the shared three-packet termination sequence to completion.
    let mut now = 100u64;
    for _ in 0..TERMINATION_PACKETS {
        assert!(removed_at(&mut source, at(now)).is_empty());
        now += 23;
    }

    // The next poll drops all three in one go, reported in ascending order.
    assert_eq!(
        removed_at(&mut source, at(now)),
        alloc::vec![univ(1), univ(2), univ(3)]
    );
}

#[test]
fn terminating_universe_without_levels_is_dropped_immediately() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    // No levels ever set.
    assert!(source.remove_universe(univ(1)));
    let (_deadline, txs) = poll_at(&mut source, at(0));
    assert_eq!(data_packets(&txs).len(), 0);
    assert!(!source.has_universe(univ(1)));
}

#[test]
fn one_transmission_per_emission_routed_to_the_universe() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[9]);

    let (_deadline, txs) = poll_at(&mut source, at(0));
    let packets = data_packets(&txs);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].route, Route::Universe(univ(1)));
}

#[test]
fn discovery_announces_active_universes() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(3)))
        .expect("should have capacity");
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(3), &[1]);
    source.update_levels(univ(1), &[1]);

    let (_deadline, txs) = poll_at(&mut source, at(0));
    let mut discovery_universes = None;
    for t in &txs {
        if matches!(t.route, Route::Discovery) {
            if let Payload::UniverseDiscovery(d) = Packet::parse(&t.bytes).unwrap().payload {
                discovery_universes = Some(d.universes.iter().collect::<Vec<_>>());
            }
        }
    }
    // Ascending order, only universes with levels.
    assert_eq!(discovery_universes.as_deref(), Some(&[1, 3][..]));
}

#[test]
fn discovery_repeats_every_interval() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);

    let had_discovery = |txs: &[OwnedTx]| txs.iter().any(|t| matches!(t.route, Route::Discovery));

    assert!(had_discovery(&poll_at(&mut source, at(0)).1));
    // Not again immediately.
    assert!(!had_discovery(&poll_at(&mut source, at(100)).1));
    // After the 10s interval.
    assert!(had_discovery(&poll_at(&mut source, at(10_000)).1));
}

#[test]
fn discovery_spans_multiple_pages() {
    let mut source = super::Source::<BigCaps>::with_config(
        SourceConfig::new(Cid::from_bytes([1; 16]), "test source").with_sync_delay(Duration::ZERO),
    );
    // One universe past a full page forces the announcement onto a second page.
    let total = MAX_UNIVERSES_PER_PAGE + 1;

    let mut order: Vec<u16> = (1..=total as u16).collect();
    fastrand::Rng::with_seed(0x5ac1).shuffle(&mut order);
    for n in order {
        source
            .add_universe(UniverseConfig::new(univ(n)))
            .expect("should have capacity");
        source.update_levels(univ(n), &[1]);
    }

    let (_deadline, txs) = poll_at(&mut source, at(0));

    // Collect the discovery pages in emission order.
    let mut pages = Vec::new();
    for t in &txs {
        if matches!(t.route, Route::Discovery) {
            if let Payload::UniverseDiscovery(d) = Packet::parse(&t.bytes).unwrap().payload {
                pages.push((
                    d.page,
                    d.last_page,
                    d.universes.iter().collect::<Vec<u16>>(),
                ));
            }
        }
    }

    // Two pages, indexed 0 then 1, each declaring last_page == 1.
    assert_eq!(pages.len(), 2, "expected two discovery pages");
    assert_eq!((pages[0].0, pages[0].1), (0, 1));
    assert_eq!((pages[1].0, pages[1].1), (1, 1));

    // Page 0 is full; page 1 carries the single remainder.
    assert_eq!(pages[0].2.len(), MAX_UNIVERSES_PER_PAGE);
    assert_eq!(pages[1].2.len(), 1);

    // The pages partition every announced universe in ascending order.
    let listed: Vec<u16> = pages
        .iter()
        .flat_map(|(_, _, universes)| universes.iter().copied())
        .collect();
    let expected: Vec<u16> = (1..=total as u16).collect();
    assert_eq!(
        listed, expected,
        "pages must partition all universes in ascending order",
    );
}

#[test]
fn add_existing_universe_is_rejected() {
    let mut source = test_source();
    assert!(source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity"));
    assert!(!source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity"));
}

#[test]
fn add_universe_reports_full_at_capacity() {
    let mut source = test_source();
    let cap = <TestCaps as SourceStorage>::TxUniverses::CAPACITY as u16;
    for u in 1..(cap + 1) {
        assert!(source
            .add_universe(UniverseConfig::new(univ(u)))
            .expect("should have capacity"));
    }
    assert_eq!(
        source
            .add_universe(UniverseConfig::new(univ(cap + 1)))
            .expect_err("capacity should be full"),
        Error::NoCapacity
    );
    // A re-add of a present universe is still distinguished from the limit.
    assert!(!source
        .add_universe(UniverseConfig::new(univ(cap)))
        .expect("should have capacity"));
}

#[test]
fn keep_alive_intervals_are_clamped_to_e131_range() {
    let cfg = SourceConfig::new(Cid::from_bytes([1; 16]), "test source");

    // Below the minimum clamps up to 800ms.
    let low = cfg
        .clone()
        .with_keep_alive(Duration::from_millis(100))
        .with_pap_keep_alive(Duration::from_millis(100));
    assert_eq!(low.keep_alive, MIN_KEEP_ALIVE);
    assert_eq!(low.pap_keep_alive, MIN_KEEP_ALIVE);

    // Above the maximum clamps down to 1000ms.
    let high = cfg
        .clone()
        .with_keep_alive(Duration::from_millis(5000))
        .with_pap_keep_alive(Duration::from_millis(5000));
    assert_eq!(high.keep_alive, MAX_KEEP_ALIVE);
    assert_eq!(high.pap_keep_alive, MAX_KEEP_ALIVE);

    // Within range is preserved.
    let mid = cfg
        .with_keep_alive(Duration::from_millis(850))
        .with_pap_keep_alive(Duration::from_millis(950));
    assert_eq!(mid.keep_alive, Duration::from_millis(850));
    assert_eq!(mid.pap_keep_alive, Duration::from_millis(950));
}

/// Polls at `now` and drains up to `limit` transmissions, then drops the
/// `SourcePoll` without draining to `None`. This models a caller cancelled after
/// `limit` sends: the drained transmissions are consumed, and the next poll
/// resumes from the un-handed-out tail (the caller owns finishing delivery of
/// any packet it was cut off mid-send on). `usize::MAX` drains fully. Returns the
/// transmissions that reached the wire.
fn poll_abandon_after(source: &mut Source, now: Instant, limit: usize) -> Vec<OwnedTx> {
    let mut poll = source.poll(now);
    let mut txs = Vec::new();
    while txs.len() < limit {
        let Some(t) = poll.next_transmission() else {
            break;
        };
        txs.push(OwnedTx {
            route: t.route,
            bytes: t.data.to_vec(),
        });
    }
    txs
}

#[test]
fn cancelled_poll_resumes_instead_of_dropping_the_tail() {
    let mut source = test_source();
    for u in [univ(1), univ(2), univ(3)] {
        source
            .add_universe(UniverseConfig::new(u))
            .expect("should have capacity");
        source.update_levels(u, &[u.get() as u8]);
    }

    // First poll has a level packet per universe (plus a discovery page). Send
    // only one of them, then cancel.
    let first = poll_abandon_after(&mut source, at(0), 1);
    assert_eq!(first.len(), 1, "only one transmission before the cancel");

    // The next poll resumes the abandoned drain rather than re-polling fresh, so
    // the packets the first poll committed but never sent still go out.
    let resumed = poll_at(&mut source, at(0)).1;
    let mut universes: Vec<u16> = first
        .iter()
        .chain(&resumed)
        .filter_map(|t| match t.route {
            Route::Universe(u) => Some(u.get()),
            Route::Discovery | Route::Sync(_) => None,
        })
        .collect();
    universes.sort_unstable();
    assert_eq!(
        universes,
        alloc::vec![1, 2, 3],
        "every universe's level was sent"
    );

    // And every level carries sequence number 0: the cancel did not burn or skip
    // a sequence number.
    for d in data_packets(&first).iter().chain(&data_packets(&resumed)) {
        assert_eq!(d.sequence, 0);
    }
}

#[test]
fn termination_completes_despite_a_cancel_on_every_poll() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[5, 6]);
    poll_at(&mut source, at(0)); // get it transmitting

    source.remove_universe(univ(1));

    let mut delivered = Vec::new();
    let mut now = 100u64;
    for _ in 0..TERMINATION_PACKETS {
        // Cancel before sending anything: the poll commits the termination
        // packet but the drain is abandoned with zero sent.
        assert!(poll_abandon_after(&mut source, at(now), 0).is_empty());
        // The retry resumes and delivers exactly the packet that was committed
        // but not sent.
        delivered.extend(data_packets(&poll_at(&mut source, at(now)).1));
        now += 23;
    }

    // All three stream-terminated packets reached the wire, with distinct
    // sequence numbers, and the universe is then dropped.
    let terminated: Vec<&OwnedData> = delivered.iter().filter(|d| d.terminated).collect();
    assert_eq!(terminated.len(), TERMINATION_PACKETS as usize);
    let mut seqs: Vec<u8> = terminated.iter().map(|d| d.sequence).collect();
    seqs.sort_unstable();
    assert_eq!(
        seqs.len(),
        TERMINATION_PACKETS as usize,
        "no sequence reuse"
    );

    poll_at(&mut source, at(now)); // physically drops the finished universe
    assert!(!source.has_universe(univ(1)));
}

#[test]
fn send_now_sends_arbitrary_start_code_in_sequence() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(3)))
        .expect("should have capacity");
    source.update_levels(univ(3), &[1, 2, 3]);

    // One scheduled level packet takes sequence 0.
    let (_d, txs) = poll_at(&mut source, at(0));
    assert_eq!(data_packets(&txs)[0].sequence, 0);

    // An ad-hoc packet takes the next sequence number (the counter is shared)
    // and carries its own start code and data.
    let tx = source
        .send_now(univ(3), StartCode::new(0x55), &[9, 9])
        .expect("send_now succeeds for a present universe");
    assert_eq!(tx.route, Route::Universe(univ(3)));
    let packet = Packet::parse(tx.data).unwrap();
    let Payload::Data(d) = packet.payload else {
        panic!("send_now produces a data packet");
    };
    assert_eq!(d.start_code, 0x55);
    assert_eq!(d.universe, 3);
    assert_eq!(d.values, &[9, 9]);
    assert_eq!(d.sequence_number.get(), 1);
}

#[test]
fn send_now_works_without_levels_and_updates_current_packet() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(2)))
        .expect("should have capacity");

    // A universe needs no level data to carry an ad-hoc start code.
    let tx = source
        .send_now(univ(2), StartCode::new(0x17), b"hello")
        .unwrap();
    let bytes = tx.data.to_vec();

    // `current_packet` re-reads the same serialized bytes without re-serializing,
    // which is how an adapter finishes an interrupted fan-out.
    assert_eq!(source.current_packet(), bytes.as_slice());

    let packet = Packet::parse(&bytes).unwrap();
    let Payload::Data(d) = packet.payload else {
        panic!("data packet");
    };
    assert_eq!(d.start_code, 0x17);
    assert_eq!(d.values, b"hello");
    assert_eq!(d.sequence_number.get(), 0);
}

#[test]
fn send_now_rejects_reserved_start_codes() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);
    assert!(matches!(
        source.send_now(univ(1), StartCode::NULL, &[1]),
        Err(Error::ReservedStartCode { start_code: 0x00 }),
    ));
    assert!(matches!(
        source.send_now(univ(1), StartCode::PAP, &[1]),
        Err(Error::ReservedStartCode { start_code: 0xDD }),
    ));
}

#[test]
fn send_now_rejects_unknown_universe() {
    let mut source = test_source();
    assert!(matches!(
        source.send_now(univ(9), StartCode::new(0x55), &[1]),
        Err(Error::NoSuchUniverse { universe: 9 }),
    ));
}

#[test]
fn send_now_rejects_oversized_data() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    let big = alloc::vec![0u8; MAX_SLOTS + 1];
    assert!(matches!(
        source.send_now(univ(1), StartCode::new(0x55), &big),
        Err(Error::Codec(_)),
    ));
}

// --- Synchronization ---------------------------------------------------------

/// The sync packets among `txs`, as `(sync_address, sequence_number)`.
fn sync_packets(txs: &[OwnedTx]) -> Vec<(u16, u8)> {
    let mut out = Vec::new();
    for t in txs {
        if let Payload::Sync(s) = Packet::parse(&t.bytes).unwrap().payload {
            assert_eq!(t.route, Route::Sync(univ(s.sync_address)));
            out.push((s.sync_address, s.sequence_number.get()));
        }
    }
    out
}

#[test]
fn synchronized_universe_emits_data_then_sync() {
    let mut source = test_source();
    source
        .add_universe(
            UniverseConfig::new(univ(1)).synchronized_on(univ(100), OnSyncLoss::HoldLastLook),
        )
        .expect("should have capacity");
    source.update_levels(univ(1), &[1, 2, 3]);

    let (_, txs) = poll_at(&mut source, at(0));
    // The data packet carries the sync address, and a sync packet follows it.
    let data = universe_data_packets(&txs);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].sync_address, 100);
    assert!(!data[0].force_sync);

    let syncs = sync_packets(&txs);
    assert_eq!(syncs, alloc::vec![(100, 0)]);

    // The sync packet is queued after the data packet.
    let data_idx = txs
        .iter()
        .position(|t| t.route == Route::Universe(univ(1)))
        .unwrap();
    let sync_idx = txs
        .iter()
        .position(|t| matches!(t.route, Route::Sync(_)))
        .unwrap();
    assert!(sync_idx > data_idx);
}

#[test]
fn one_sync_sent_for_a_multi_universe_group() {
    let mut source = test_source();
    for u in [1, 2, 3] {
        source
            .add_universe(
                UniverseConfig::new(univ(u)).synchronized_on(univ(50), OnSyncLoss::HoldLastLook),
            )
            .expect("should have capacity");
        source.update_levels(univ(u), &[u as u8]);
    }

    let (_, txs) = poll_at(&mut source, at(0));
    assert_eq!(universe_data_packets(&txs).len(), 3);
    assert_eq!(sync_packets(&txs), alloc::vec![(50, 0)]);
}

#[test]
fn sync_sequence_is_independent_of_data_and_advances_per_group() {
    let mut source = test_source();
    source
        .add_universe(
            UniverseConfig::new(univ(1)).synchronized_on(univ(100), OnSyncLoss::HoldLastLook),
        )
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);

    // The pre-suppression burst produces a sync per data packet; the sync
    // group has its own sequence number starting at 0.
    let (_, a) = poll_at(&mut source, at(0));
    let (_, b) = poll_at(&mut source, at(22));
    let (_, c) = poll_at(&mut source, at(44));
    assert_eq!(sync_packets(&a), alloc::vec![(100, 0)]);
    assert_eq!(sync_packets(&b), alloc::vec![(100, 1)]);
    assert_eq!(sync_packets(&c), alloc::vec![(100, 2)]);
}

#[test]
fn sync_delay_defers_the_sync_to_a_later_poll() {
    let mut source = Source::new(
        SourceConfig::new(Cid::from_bytes([1; 16]), "test")
            .with_sync_delay(Duration::from_millis(5)),
    );
    source
        .add_universe(
            UniverseConfig::new(univ(1)).synchronized_on(univ(100), OnSyncLoss::HoldLastLook),
        )
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);

    // The data goes out now, but the sync is armed for now + 5ms.
    let (deadline, txs) = poll_at(&mut source, at(0));
    assert_eq!(universe_data_packets(&txs).len(), 1);
    assert!(sync_packets(&txs).is_empty());
    assert_eq!(deadline, Some(at(5)));

    // At the deadline the sync fires.
    let (_, txs) = poll_at(&mut source, at(5));
    assert_eq!(sync_packets(&txs), alloc::vec![(100, 0)]);
}

#[test]
fn revert_to_live_sets_force_sync_bit() {
    let mut source = test_source();
    source
        .add_universe(
            UniverseConfig::new(univ(1)).synchronized_on(univ(100), OnSyncLoss::RevertToLive),
        )
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);

    let (_, txs) = poll_at(&mut source, at(0));
    let data = universe_data_packets(&txs);
    assert_eq!(data.len(), 1);
    assert!(data[0].force_sync);
    assert_eq!(data[0].sync_address, 100);
}

#[test]
fn termination_desynchronizes() {
    let mut source = test_source();
    source
        .add_universe(
            UniverseConfig::new(univ(1)).synchronized_on(univ(100), OnSyncLoss::HoldLastLook),
        )
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);
    let _ = poll_at(&mut source, at(0));

    source.remove_universe(univ(1));
    let (_, txs) = poll_at(&mut source, at(22));
    let data = universe_data_packets(&txs);
    assert!(!data.is_empty());
    for d in &data {
        assert!(d.stream_terminated);
        assert_eq!(d.sync_address, 0);
    }
    assert!(sync_packets(&txs).is_empty());
}

#[test]
fn terminating_a_universe_drops_its_deferred_sync() {
    let mut source = Source::new(
        SourceConfig::new(Cid::from_bytes([1; 16]), "test")
            .with_sync_delay(Duration::from_millis(5)),
    );
    source
        .add_universe(
            UniverseConfig::new(univ(1)).synchronized_on(univ(100), OnSyncLoss::HoldLastLook),
        )
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);

    // Data goes out and the sync is armed for now + 5ms, but not yet sent.
    let (_, txs) = poll_at(&mut source, at(0));
    assert!(sync_packets(&txs).is_empty());

    // Terminate before the deferred sync's deadline elapses.
    source.remove_universe(univ(1));

    // At and past the old sync deadline, no sync ever fires: the group went away
    // with its universe instead of firing for a terminated stream.
    for t in [5, 10, 22, 44] {
        let (_, txs) = poll_at(&mut source, at(t));
        assert!(
            sync_packets(&txs).is_empty(),
            "a terminated universe must not emit a deferred sync (poll at {t}ms)"
        );
    }
}

#[test]
fn set_synchronization_starts_and_stops() {
    let mut source = test_source();
    source
        .add_universe(UniverseConfig::new(univ(1)))
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);
    let (_, txs) = poll_at(&mut source, at(0));
    assert_eq!(universe_data_packets(&txs)[0].sync_address, 0);
    assert!(sync_packets(&txs).is_empty());

    // Start synchronization at runtime.
    source.set_synchronization(univ(1), Some((univ(100), OnSyncLoss::HoldLastLook)));
    let (_, txs) = poll_at(&mut source, at(22));
    assert_eq!(universe_data_packets(&txs)[0].sync_address, 100);
    assert_eq!(sync_packets(&txs), alloc::vec![(100, 0)]);

    // Stop synchronization: data reverts to sync_address 0 and no more syncs.
    source.set_synchronization(univ(1), None);
    let (_, txs) = poll_at(&mut source, at(44));
    assert_eq!(universe_data_packets(&txs)[0].sync_address, 0);
    assert!(sync_packets(&txs).is_empty());
}

#[test]
fn discovery_lists_sync_universes() {
    let mut source = test_source();
    source
        .add_universe(
            UniverseConfig::new(univ(1)).synchronized_on(univ(100), OnSyncLoss::HoldLastLook),
        )
        .expect("should have capacity");
    source.update_levels(univ(1), &[1]);

    let (_, txs) = poll_at(&mut source, at(0));
    let listed: Vec<u16> = txs
        .iter()
        .filter_map(|t| match Packet::parse(&t.bytes).unwrap().payload {
            Payload::UniverseDiscovery(d) => Some(d.universes.iter().collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(listed, alloc::vec![1, 100]);
}
