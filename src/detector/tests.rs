//! Unit tests for the source detector.

use super::*;
use crate::packet::{Packet, Payload, UniverseDiscoveryPacket, UniverseList};
use crate::time::Duration;

use alloc::vec;
use alloc::vec::Vec;

// --- Helpers -----------------------------------------------------------------

fn instant(ms: u64) -> Instant {
    Instant::from_epoch(Duration::from_millis(ms))
}

fn cid(n: u8) -> Cid {
    Cid::from_bytes([n; 16])
}

/// Encodes a slice of universes as the big-endian bytes a
/// [`UniverseList`] borrows.
fn universe_bytes(universes: &[u16]) -> Vec<u8> {
    universes.iter().flat_map(|u| u.to_be_bytes()).collect()
}

/// Builds a universe discovery packet from its parts, borrowing `bytes` for the
/// universe list.
fn discovery<'a>(
    source: Cid,
    name: &'a str,
    page: u8,
    last_page: u8,
    bytes: &'a [u8],
) -> Packet<'a> {
    Packet {
        cid: source,
        payload: Payload::UniverseDiscovery(UniverseDiscoveryPacket {
            source_name: name,
            page,
            last_page,
            universes: UniverseList::from_bytes(bytes),
        }),
    }
}

/// Feeds a single-page discovery packet and returns the owned events produced.
fn feed_page(
    d: &mut SourceDetector,
    ms: u64,
    source: Cid,
    name: &str,
    page: u8,
    last_page: u8,
    universes: &[u16],
) -> Vec<SourceDetectorEvent> {
    let bytes = universe_bytes(universes);
    let packet = discovery(source, name, page, last_page, &bytes);
    let mut events = Vec::new();
    d.handle_packet(instant(ms), &packet)
        .for_each_owned(|event| events.push(event));
    events
}

/// Feeds a complete single-page (page 0 of 0) list and returns the events.
fn feed(
    d: &mut SourceDetector,
    ms: u64,
    source: Cid,
    name: &str,
    universes: &[u16],
) -> Vec<SourceDetectorEvent> {
    feed_page(d, ms, source, name, 0, 0, universes)
}

/// Advances the clock and returns the owned poll events produced.
fn poll(d: &mut SourceDetector, ms: u64) -> Vec<SourceDetectorEvent> {
    d.poll(instant(ms))
        .events()
        .iter()
        .map(Into::into)
        .collect()
}

fn new_detector() -> SourceDetector {
    SourceDetector::new(SourceDetectorConfig::new())
}

// --- New source / updates ----------------------------------------------------

#[test]
fn new_source_is_reported_with_its_universes() {
    let mut d = new_detector();
    let events = feed(&mut d, 0, cid(1), "src", &[1, 2, 3]);
    assert_eq!(
        events,
        vec![SourceDetectorEvent::SourceUpdated {
            cid: cid(1),
            name: "src".into(),
            universes: vec![1, 2, 3],
        }]
    );
}

#[test]
fn unchanged_list_is_not_reported_again() {
    let mut d = new_detector();
    assert_eq!(feed(&mut d, 0, cid(1), "src", &[1, 2, 3]).len(), 1);
    // Re-announcing the same list refreshes the source but reports nothing.
    assert!(feed(&mut d, 1000, cid(1), "src", &[1, 2, 3]).is_empty());
}

#[test]
fn changed_list_is_reported() {
    let mut d = new_detector();
    feed(&mut d, 0, cid(1), "src", &[1, 2, 3]);
    let events = feed(&mut d, 1000, cid(1), "src", &[1, 2, 3, 4]);
    assert_eq!(
        events,
        vec![SourceDetectorEvent::SourceUpdated {
            cid: cid(1),
            name: "src".into(),
            universes: vec![1, 2, 3, 4],
        }]
    );
}

#[test]
fn shrinking_to_empty_is_reported() {
    let mut d = new_detector();
    feed(&mut d, 0, cid(1), "src", &[1, 2, 3]);
    let events = feed(&mut d, 1000, cid(1), "src", &[]);
    assert_eq!(
        events,
        vec![SourceDetectorEvent::SourceUpdated {
            cid: cid(1),
            name: "src".into(),
            universes: vec![],
        }]
    );
}

#[test]
fn new_source_with_no_universes_is_not_reported() {
    // A source that announces an empty list on first sight has nothing to
    // report - its "list" already matches the empty starting state.
    let mut d = new_detector();
    assert!(feed(&mut d, 0, cid(1), "src", &[]).is_empty());
}

#[test]
fn non_discovery_packets_are_ignored() {
    use crate::packet::SyncPacket;
    use crate::types::SequenceNumber;

    let mut d = new_detector();
    let packet = Packet {
        cid: cid(1),
        payload: Payload::Sync(SyncPacket {
            sequence_number: SequenceNumber::new(0),
            sync_address: 1,
        }),
    };
    let outcome = d.handle_packet(instant(0), &packet);
    assert!(outcome.updated.is_none() && outcome.limit_exceeded.is_none());
    assert_eq!(d.next_deadline(), None, "no source should be tracked");
}

#[test]
fn name_change_is_reflected_in_expiry_notification() {
    let mut d = new_detector();
    feed(&mut d, 0, cid(1), "old name", &[1]);
    // Re-announce with a new name and the same list (no update event).
    assert!(feed(&mut d, 1000, cid(1), "new name", &[1]).is_empty());
    let events = poll(&mut d, 30_000);
    assert_eq!(
        events,
        vec![SourceDetectorEvent::SourceExpired {
            cid: cid(1),
            name: "new name".into(),
        }]
    );
}

// --- Page reassembly ---------------------------------------------------------

#[test]
fn multi_page_list_is_reassembled_on_last_page() {
    let mut d = new_detector();
    // Page 0 of 2: nothing yet.
    assert!(feed_page(&mut d, 0, cid(1), "src", 0, 2, &[1, 2]).is_empty());
    // Page 1 of 2: still nothing.
    assert!(feed_page(&mut d, 10, cid(1), "src", 1, 2, &[3, 4]).is_empty());
    // Page 2 of 2 completes the list.
    let events = feed_page(&mut d, 20, cid(1), "src", 2, 2, &[5, 6]);
    assert_eq!(
        events,
        vec![SourceDetectorEvent::SourceUpdated {
            cid: cid(1),
            name: "src".into(),
            universes: vec![1, 2, 3, 4, 5, 6],
        }]
    );
}

#[test]
fn out_of_order_page_restarts_reassembly() {
    let mut d = new_detector();
    feed_page(&mut d, 0, cid(1), "src", 0, 2, &[1, 2]);
    // A page 2 arriving before page 1 is out of sequence: discard the partial.
    assert!(feed_page(&mut d, 10, cid(1), "src", 2, 2, &[5, 6]).is_empty());
    // Nothing completes until a fresh page-0 sequence arrives.
    assert!(feed_page(&mut d, 20, cid(1), "src", 1, 2, &[3, 4]).is_empty());
    // Restart cleanly.
    feed_page(&mut d, 30, cid(1), "src", 0, 1, &[10, 20]);
    let events = feed_page(&mut d, 40, cid(1), "src", 1, 1, &[30, 40]);
    assert_eq!(
        events,
        vec![SourceDetectorEvent::SourceUpdated {
            cid: cid(1),
            name: "src".into(),
            universes: vec![10, 20, 30, 40],
        }]
    );
}

#[test]
fn restarting_at_page_zero_mid_sequence_discards_partial() {
    let mut d = new_detector();
    feed_page(&mut d, 0, cid(1), "src", 0, 1, &[99]);
    // A fresh page 0 abandons the incomplete sequence rather than appending.
    let events = feed_page(&mut d, 10, cid(1), "src", 0, 0, &[1, 2, 3]);
    assert_eq!(
        events,
        vec![SourceDetectorEvent::SourceUpdated {
            cid: cid(1),
            name: "src".into(),
            universes: vec![1, 2, 3],
        }]
    );
}

#[test]
fn non_ascending_list_is_dropped() {
    let mut d = new_detector();
    // Out-of-order (non-conformant) universe list: not reported.
    assert!(feed(&mut d, 0, cid(1), "src", &[3, 1, 2]).is_empty());
    // Duplicates are also non-ascending.
    assert!(feed(&mut d, 10, cid(1), "src", &[1, 1, 2]).is_empty());
    // A valid list afterwards is reported.
    let events = feed(&mut d, 20, cid(1), "src", &[1, 2, 3]);
    assert_eq!(events.len(), 1);
}

#[test]
fn page_index_past_last_page_is_ignored() {
    let mut d = new_detector();
    // page 3 with last_page 1 is malformed.
    assert!(feed_page(&mut d, 0, cid(1), "src", 3, 1, &[1, 2]).is_empty());
    // The source is tracked (expiry refreshed) but has no universes yet.
    assert_eq!(d.next_deadline(), Some(instant(20_000)));
}

// --- Expiry ------------------------------------------------------------------

#[test]
fn source_expires_after_timeout() {
    let mut d = new_detector();
    feed(&mut d, 0, cid(1), "src", &[1]);
    // Nothing expires before the timeout.
    assert!(poll(&mut d, 19_999).is_empty());
    // At the timeout, the source expires.
    let events = poll(&mut d, 20_000);
    assert_eq!(
        events,
        vec![SourceDetectorEvent::SourceExpired {
            cid: cid(1),
            name: "src".into(),
        }]
    );
    // It is gone: no deadline remains.
    assert_eq!(d.next_deadline(), None);
}

#[test]
fn announcement_refreshes_expiry() {
    let mut d = new_detector();
    feed(&mut d, 0, cid(1), "src", &[1]);
    // Re-announce close to the deadline; expiry moves out to 15_000 + 20_000.
    feed(&mut d, 15_000, cid(1), "src", &[1]);
    assert!(poll(&mut d, 20_000).is_empty());
    let events = poll(&mut d, 35_000);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        SourceDetectorEvent::SourceExpired { .. }
    ));
}

#[test]
fn configurable_timeout_is_respected() {
    let config = SourceDetectorConfig::new().with_source_timeout(Duration::from_millis(5000));
    let mut d = SourceDetector::new(config);
    feed(&mut d, 0, cid(1), "src", &[1]);
    assert!(poll(&mut d, 4999).is_empty());
    assert_eq!(poll(&mut d, 5000).len(), 1);
}

#[test]
fn multiple_sources_are_tracked_independently() {
    let mut d = new_detector();
    feed(&mut d, 0, cid(1), "a", &[1]);
    feed(&mut d, 5000, cid(2), "b", &[2]);
    // The nearest deadline is source 1's.
    assert_eq!(d.next_deadline(), Some(instant(20_000)));
    let expired = poll(&mut d, 20_000);
    assert_eq!(
        expired,
        vec![SourceDetectorEvent::SourceExpired {
            cid: cid(1),
            name: "a".into(),
        }]
    );
    // Source 2 remains, expiring later.
    assert_eq!(d.next_deadline(), Some(instant(25_000)));
}

// --- Limits ------------------------------------------------------------------

#[test]
fn source_limit_is_enforced_and_rate_limited() {
    let config = SourceDetectorConfig::new().with_source_limit(1);
    let mut d = SourceDetector::new(config);
    assert_eq!(feed(&mut d, 0, cid(1), "a", &[1]).len(), 1);

    // A second source cannot be tracked: one limit-exceeded, then suppressed.
    assert_eq!(
        feed(&mut d, 100, cid(2), "b", &[2]),
        vec![SourceDetectorEvent::SourceLimitExceeded]
    );
    assert!(feed(&mut d, 200, cid(3), "c", &[3]).is_empty());

    // Once the tracked source expires, a fresh notification is allowed.
    poll(&mut d, 20_000);
    assert_eq!(
        feed(&mut d, 20_100, cid(4), "d", &[4]).len(),
        1,
        "the new source now fits and is reported"
    );
    assert_eq!(
        feed(&mut d, 20_200, cid(5), "e", &[5]),
        vec![SourceDetectorEvent::SourceLimitExceeded]
    );
}

#[test]
fn universe_limit_truncates_and_reports() {
    let config = SourceDetectorConfig::new().with_universes_per_source_limit(2);
    let mut d = SourceDetector::new(config);
    let events = feed(&mut d, 0, cid(1), "src", &[1, 2, 3, 4]);
    assert_eq!(
        events,
        vec![
            SourceDetectorEvent::UniverseLimitExceeded { cid: cid(1) },
            SourceDetectorEvent::SourceUpdated {
                cid: cid(1),
                name: "src".into(),
                universes: vec![1, 2],
            }
        ]
    );
    // Re-announcing an over-limit list is suppressed (same truncated result).
    assert!(feed(&mut d, 1000, cid(1), "src", &[1, 2, 3, 4]).is_empty());
}

#[test]
fn universe_limit_spans_pages() {
    let config = SourceDetectorConfig::new().with_universes_per_source_limit(3);
    let mut d = SourceDetector::new(config);
    // Page 0 fills two of three slots.
    assert!(feed_page(&mut d, 0, cid(1), "src", 0, 1, &[1, 2]).is_empty());
    // Page 1 overflows: only universe 3 fits, the rest is dropped.
    let events = feed_page(&mut d, 10, cid(1), "src", 1, 1, &[3, 4, 5]);
    assert_eq!(
        events,
        vec![
            SourceDetectorEvent::UniverseLimitExceeded { cid: cid(1) },
            SourceDetectorEvent::SourceUpdated {
                cid: cid(1),
                name: "src".into(),
                universes: vec![1, 2, 3],
            }
        ]
    );
}

#[test]
fn universe_limit_names_the_overflowing_source_independently() {
    let config = SourceDetectorConfig::new().with_universes_per_source_limit(2);
    let mut d = SourceDetector::new(config);

    // Source 1 stays within its limit and is reported normally.
    assert_eq!(
        feed(&mut d, 0, cid(1), "a", &[1, 2]),
        vec![SourceDetectorEvent::SourceUpdated {
            cid: cid(1),
            name: "a".into(),
            universes: vec![1, 2],
        }]
    );

    // Source 2 overflows: the limit-exceeded notification names source 2, not
    // the other tracked source.
    assert_eq!(
        feed(&mut d, 10, cid(2), "b", &[10, 11, 12]),
        vec![
            SourceDetectorEvent::UniverseLimitExceeded { cid: cid(2) },
            SourceDetectorEvent::SourceUpdated {
                cid: cid(2),
                name: "b".into(),
                universes: vec![10, 11],
            }
        ]
    );

    // Source 2's overflow is suppressed on re-announce, but suppression is
    // per-source: source 1 overflowing now still produces its own notification.
    assert!(feed(&mut d, 20, cid(2), "b", &[10, 11, 12]).is_empty());
    assert_eq!(
        feed(&mut d, 30, cid(1), "a", &[5, 6, 7]),
        vec![
            SourceDetectorEvent::UniverseLimitExceeded { cid: cid(1) },
            SourceDetectorEvent::SourceUpdated {
                cid: cid(1),
                name: "a".into(),
                universes: vec![5, 6],
            }
        ]
    );
}
