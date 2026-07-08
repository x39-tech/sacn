//! Tests for the DMX merger.
//!
//! Each group of tests uses a fixed capacity policy equal to the maximum number
//! of concurrent sources that group reaches. This tests the boundary conditions
//! for the fixed-capacity storage at the same time as the actual test logic is
//! exercised. The regular tests use [`UnitCaps`]; the differential property
//! test uses [`PropCaps`].
//!
//! Note that the test module still requires `alloc` because it is used for test
//! scaffolding.

use alloc::vec::Vec;

use proptest::collection::vec;
use proptest::prelude::*;

use super::{
    universe_priority_to_pap, MergeSourceEntry, OutBuf, SlotOwner, SourceId, SourceTable,
    MAX_SLOTS, NO_OWNER,
};
use crate::error::Error;
use crate::static_storage;
use crate::types::Priority;

// --- test storage policies ---------------------------------------------------

// Maximum merge sources used across the unit tests
const UNIT_MAX_SOURCES: usize = 3;

static_storage! {
    struct UnitCaps {
        rx_universes: 0,
        rx_sources_per_universe: UNIT_MAX_SOURCES,
        rx_sync_addresses: 0,
        tx_universes: 0,
        det_sources: 0,
        det_universes_per_source: 0
    }
}

type DmxMerger = super::DmxMerger<UnitCaps>;

impl DmxMerger {
    fn new() -> Self {
        Self::default()
    }
}

// --- unit tests -------------------------------------------------------------

fn priority(p: u8) -> Priority {
    Priority::new(p).unwrap()
}

#[test]
fn htp_among_equal_priority() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    let b = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_universe_priority(b, priority(100));

    m.update_levels(a, &[100, 100, 100]);
    m.update_levels(b, &[50, 200, 100]);

    let out = m.output();
    // Slot 0: A higher. Slot 1: B higher. Slot 2: tie -> lower id (A) wins.
    assert_eq!(&out.levels()[..3], &[100, 200, 100]);
    assert_eq!(&out.priorities()[..3], &[100, 100, 100]);
    let owners = out.owners();
    assert_eq!(owners[0].source(), Some(a));
    assert_eq!(owners[1].source(), Some(b));
    assert_eq!(owners[2].source(), Some(a));
}

#[test]
fn deferred_holds_output_frozen_until_cleared() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_levels(a, &[10, 20, 30]);
    assert_eq!(&m.output().levels()[..3], &[10, 20, 30]);

    // While deferred, updates are applied to source state but the output stays
    // frozen at the last computed frame.
    m.set_deferred(true);
    m.update_levels(a, &[40, 50, 60]);
    assert_eq!(&m.output().levels()[..3], &[10, 20, 30]);

    // Clearing deferral publishes the accumulated frame in one recompute.
    m.set_deferred(false);
    assert_eq!(&m.output().levels()[..3], &[40, 50, 60]);
}

#[test]
fn recompute_updates_without_clearing_deferral() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_levels(a, &[1, 2, 3]);

    m.set_deferred(true);
    m.update_levels(a, &[7, 8, 9]);
    // recompute publishes the current state but leaves further updates deferred.
    m.recompute();
    assert!(m.is_deferred());
    assert_eq!(&m.output().levels()[..3], &[7, 8, 9]);

    m.update_levels(a, &[100, 100, 100]);
    assert_eq!(&m.output().levels()[..3], &[7, 8, 9]);
}

#[test]
fn higher_priority_beats_higher_level() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    let b = m.add_source().unwrap();
    m.update_universe_priority(a, priority(150));
    m.update_universe_priority(b, priority(100));

    m.update_levels(a, &[10]);
    m.update_levels(b, &[255]);

    let out = m.output();
    // A wins despite a far lower level, because its priority is higher.
    assert_eq!(out.levels()[0], 10);
    assert_eq!(out.priorities()[0], 150);
    assert_eq!(out.owners()[0].source(), Some(a));
}

#[test]
fn owner_release_falls_back_to_next_source() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    let b = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_universe_priority(b, priority(100));

    m.update_levels(a, &[100]);
    m.update_levels(b, &[200]); // B owns at 200
    assert_eq!(m.output().owners()[0].source(), Some(b));

    m.update_levels(b, &[50]); // B drops below A: A reclaims
    let out = m.output();
    assert_eq!(out.levels()[0], 100);
    assert_eq!(out.owners()[0].source(), Some(a));
}

#[test]
fn universe_priority_zero_becomes_pap_one() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    m.update_universe_priority(a, priority(0));
    m.update_levels(a, &[42]);

    let out = m.output();
    assert_eq!(
        out.priorities()[0],
        1,
        "universe priority 0 must convert to PAP 1"
    );
    assert_eq!(out.owners()[0].source(), Some(a));
    assert_eq!(out.levels()[0], 42);
}

#[test]
fn per_address_priority_overrides_universe_priority() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    let b = m.add_source().unwrap();
    // A has a high universe priority; B beats it on slot 0 only via explicit PAP.
    m.update_universe_priority(a, priority(150));
    m.update_levels(a, &[100, 100]);
    m.update_universe_priority(b, priority(100));
    m.update_levels(b, &[10, 10]);
    m.update_per_address_priorities(b, &[200, 0]);

    let out = m.output();
    // Slot 0: B's PAP (200) outranks A (150). Slot 1: B is not sourcing (PAP 0),
    // so A wins.
    assert_eq!(out.owners()[0].source(), Some(b));
    assert_eq!(out.priorities()[0], 200);
    assert_eq!(out.levels()[0], 10);
    assert_eq!(out.owners()[1].source(), Some(a));
    assert_eq!(out.priorities()[1], 150);

    // Removing PAP reverts B to its universe priority, handing slot 0 to A.
    m.remove_per_address_priorities(b);
    let out = m.output();
    assert_eq!(out.owners()[0].source(), Some(a));
    assert_eq!(out.priorities()[0], 150);
}

#[test]
fn shorter_pap_packet_releases_trailing_slots() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    m.update_levels(a, &[10, 20, 30]);
    m.update_per_address_priorities(a, &[100, 100, 100]);
    let out = m.output();
    let owners = out.owners();
    assert_eq!(owners[0].source(), Some(a));
    assert_eq!(owners[1].source(), Some(a));
    assert_eq!(owners[2].source(), Some(a));

    // A shorter PAP packet zeroes the trailing PAP, so those slots go unsourced.
    m.update_per_address_priorities(a, &[100]);
    let out = m.output();
    assert_eq!(out.owners()[0].source(), Some(a));
    assert!(out.owners()[1].is_none());
    assert!(out.owners()[2].is_none());
    assert_eq!(out.levels()[1], 0);
}

#[test]
fn removing_a_source_releases_its_slots() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    let b = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_universe_priority(b, priority(120));
    m.update_levels(a, &[50]);
    m.update_levels(b, &[60]);
    assert_eq!(m.output().owners()[0].source(), Some(b));

    m.remove_source(b);
    let out = m.output();
    assert_eq!(
        out.owners()[0].source(),
        Some(a),
        "A reclaims the slot B held"
    );
    assert_eq!(out.priorities()[0], 100);
    assert_eq!(out.levels()[0], 50);
}

// --- pinned tie-break contract -----------------------------------------------

#[test]
fn breaks_priority_level_tie_toward_lowest_index() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    let b = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_universe_priority(b, priority(100));
    m.update_levels(b, &[100]); // b (higher index) seizes the slot first
    m.update_levels(a, &[100]); // a (lower index) ties it exactly

    let out = m.output();
    assert_eq!(out.levels()[0], 100);
    assert_eq!(out.priorities()[0], 100);
    assert_eq!(
        out.owners()[0].source(),
        Some(a),
        "a priority+level tie must go to the lowest source index"
    );
}

// --- source lifecycle & index reuse ------------------------------------------

#[test]
fn freed_index_is_reused_and_reinserted_in_order() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap(); // index 0
    let b = m.add_source().unwrap(); // index 1
    let c = m.add_source().unwrap(); // index 2
    assert_eq!((a.index(), b.index(), c.index()), (0, 1, 2));

    m.remove_source(b); // frees index 1
    let d = m.add_source().unwrap();
    assert_eq!(
        d.index(),
        1,
        "a freed index is handed to the next add_source"
    );

    // The reused index reinserts into the middle of the sorted table; all
    // three live sources still merge correctly afterward.
    m.update_universe_priority(a, priority(100));
    m.update_universe_priority(c, priority(100));
    m.update_universe_priority(d, priority(150));
    m.update_levels(a, &[10]);
    m.update_levels(c, &[20]);
    m.update_levels(d, &[30]);

    let out = m.output();
    assert_eq!(out.owners()[0].source(), Some(d));
    assert_eq!(out.levels()[0], 30);
    assert_eq!(out.priorities()[0], 150);
}

#[test]
fn reused_low_index_wins_ties_against_older_sources() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap(); // index 0
    let b = m.add_source().unwrap(); // index 1
    m.update_universe_priority(a, priority(100));
    m.update_universe_priority(b, priority(100));
    m.update_levels(a, &[100]);
    m.update_levels(b, &[100]);
    // Tie -> lowest index (a) owns.
    assert_eq!(m.output().owners()[0].source(), Some(a));

    // Remove a, freeing index 0; b (index 1) now owns the slot alone.
    m.remove_source(a);
    assert_eq!(m.output().owners()[0].source(), Some(b));

    // A brand-new source reuses index 0, so on an exact priority+level tie it
    // beats the older b (index 1).
    let c = m.add_source().unwrap();
    assert_eq!(c.index(), 0);
    m.update_universe_priority(c, priority(100));
    m.update_levels(c, &[100]);

    assert_eq!(
        m.output().owners()[0].source(),
        Some(c),
        "a reused low index wins a priority+level tie against an older source"
    );
}

#[test]
fn stale_handle_is_ignored_then_aliases_the_reusing_source() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap(); // index 0
    m.update_universe_priority(a, priority(100));
    m.update_levels(a, &[50]);
    assert_eq!(m.output().owners()[0].source(), Some(a));

    m.remove_source(a);
    // The handle is now stale: updates through it are silently ignored.
    m.update_levels(a, &[200]);
    assert!(m.output().owners()[0].is_none());

    // A new source reuses index 0, so the old handle now addresses the *new*
    // source (handles are indices, reused like Vec slots).
    let b = m.add_source().unwrap();
    assert_eq!(a.index(), b.index());
    m.update_universe_priority(a, priority(100)); // resolves to b
    m.update_levels(b, &[77]);
    let out = m.output();
    assert_eq!(out.owners()[0].source(), Some(b));
    assert_eq!(out.levels()[0], 77);
}

#[test]
fn removing_the_last_source_zeroes_the_output() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_levels(a, &[10, 20, 30]);
    assert!(m.output().owners()[0].is_some());

    m.remove_source(a);
    let out = m.output();
    assert!(out.levels().iter().all(|&l| l == 0));
    assert!(out.priorities().iter().all(|&p| p == 0));
    assert!(out.owners().iter().all(|o| o.is_none()));
}

// --- slot-count boundaries ---------------------------------------------------

#[test]
fn merges_a_full_width_universe() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    let b = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_universe_priority(b, priority(100));

    let asc: Vec<u8> = (0..MAX_SLOTS).map(|i| (i % 256) as u8).collect();
    let desc: Vec<u8> = (0..MAX_SLOTS).map(|i| (255 - (i % 256)) as u8).collect();
    m.update_levels(a, &asc);
    m.update_levels(b, &desc);

    let out = m.output();
    for slot in 0..MAX_SLOTS {
        assert_eq!(out.levels()[slot], asc[slot].max(desc[slot]), "slot {slot}");
        assert_eq!(out.priorities()[slot], 100, "slot {slot}");
        // Equal levels never occur here, but `>=` keeps the tie going to a.
        let winner = if asc[slot] >= desc[slot] { a } else { b };
        assert_eq!(out.owners()[slot].source(), Some(winner), "slot {slot}");
    }
}

#[test]
fn levels_and_pap_beyond_one_universe_are_truncated() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();

    // Oversized buffers: only the first MAX_SLOTS entries are read. The
    // truncated tail is given distinct values so a regression that read past
    // the universe would change the result.
    let mut levels = alloc::vec![7u8; MAX_SLOTS + 100];
    let mut pap = alloc::vec![200u8; MAX_SLOTS + 100];
    levels[MAX_SLOTS..].fill(255);
    pap[MAX_SLOTS..].fill(1);

    m.update_levels(a, &levels);
    m.update_per_address_priorities(a, &pap);

    let out = m.output();
    assert_eq!(out.levels().len(), MAX_SLOTS);
    assert!(out.levels().iter().all(|&l| l == 7));
    assert!(out.priorities().iter().all(|&p| p == 200));
    assert!(out.owners().iter().all(|o| o.source() == Some(a)));
}

#[test]
fn empty_level_or_pap_update_stops_sourcing() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_levels(a, &[10, 20, 30]);
    assert!(m.output().owners()[0].is_some());

    // An empty level update drops the valid level count to zero: the source
    // sources nothing, even though its universe priority is still set.
    m.update_levels(a, &[]);
    let out = m.output();
    assert!(out.owners().iter().all(|o| o.is_none()));
    assert!(out.levels().iter().all(|&l| l == 0));

    // Restore levels, then an empty PAP update likewise stops sourcing: every
    // slot's per-address priority is now 0, i.e. "not sourcing".
    m.update_levels(a, &[10, 20, 30]);
    assert!(m.output().owners()[0].is_some());
    m.update_per_address_priorities(a, &[]);
    assert!(m.output().owners().iter().all(|o| o.is_none()));
}

#[test]
fn shorter_level_packet_releases_trailing_slots() {
    let mut m = DmxMerger::new();
    let a = m.add_source().unwrap();
    m.update_universe_priority(a, priority(100));
    m.update_levels(a, &[10, 20, 30]);
    let out = m.output();
    assert_eq!(out.owners()[0].source(), Some(a));
    assert_eq!(out.owners()[1].source(), Some(a));
    assert_eq!(out.owners()[2].source(), Some(a));

    // A shorter level packet shrinks the valid level count; the trailing slots
    // are released and their levels cleared (the levels-side mirror of
    // `shorter_pap_packet_releases_trailing_slots`).
    m.update_levels(a, &[10]);
    let out = m.output();
    assert_eq!(out.owners()[0].source(), Some(a));
    assert!(out.owners()[1].is_none());
    assert!(out.owners()[2].is_none());
    assert_eq!(out.levels()[1], 0);
    assert_eq!(out.levels()[2], 0);
}

#[test]
fn adding_past_capacity_reports_no_capacity() {
    let mut m = DmxMerger::new();
    for _ in 0..UNIT_MAX_SOURCES {
        m.add_source().unwrap();
    }
    assert_eq!(m.add_source(), Err(Error::NoCapacity));
}

#[test]
fn capacity_frees_on_remove() {
    let mut m = DmxMerger::new();
    let mut a = SourceId(NO_OWNER);
    for _ in 0..UNIT_MAX_SOURCES {
        a = m.add_source().unwrap();
    }

    assert_eq!(m.add_source(), Err(Error::NoCapacity));
    m.remove_source(a);
    assert!(m.add_source().is_ok());
}

// --- the shared merger interface for the oracle ------------------------------

static_storage! {
    struct PropCaps {
        rx_universes: 0,
        // Peak concurrent sources the differential property test can reach: its initial
        // count (1-4) plus one per `AddSource` op (1-80), with no removes. See
        rx_sources_per_universe: 4 + 80,
        rx_sync_addresses: 0,
        tx_universes: 0,
        det_sources: 0,
        det_universes_per_source: 0

    }
}

/// The merger under test in the differential property test, bound to
/// [`PropCaps`].
type PropMerger = super::DmxMerger<PropCaps>;

/// The merge operations the oracle drives, abstracted over the shipped merger
/// and the [`Naive`] reference.
trait TestMerger {
    fn create() -> Self;
    fn add_source(&mut self) -> SourceId;
    fn remove_source(&mut self, id: SourceId);
    fn update_levels(&mut self, id: SourceId, levels: &[u8]);
    fn update_pap(&mut self, id: SourceId, pap: &[u8]);
    fn update_universe_priority(&mut self, id: SourceId, priority: Priority);
    fn remove_pap(&mut self, id: SourceId);
    fn levels(&self) -> &[u8];
    fn priorities(&self) -> &[u8];
    fn owners(&self) -> &[SlotOwner];
}

impl TestMerger for PropMerger {
    fn create() -> Self {
        Self::default()
    }
    fn add_source(&mut self) -> SourceId {
        PropMerger::add_source(self).unwrap()
    }
    fn remove_source(&mut self, id: SourceId) {
        PropMerger::remove_source(self, id);
    }
    fn update_levels(&mut self, id: SourceId, levels: &[u8]) {
        PropMerger::update_levels(self, id, levels);
    }
    fn update_pap(&mut self, id: SourceId, pap: &[u8]) {
        PropMerger::update_per_address_priorities(self, id, pap);
    }
    fn update_universe_priority(&mut self, id: SourceId, priority: Priority) {
        PropMerger::update_universe_priority(self, id, priority);
    }
    fn remove_pap(&mut self, id: SourceId) {
        PropMerger::remove_per_address_priorities(self, id);
    }
    fn levels(&self) -> &[u8] {
        self.out.levels.as_slice()
    }
    fn priorities(&self) -> &[u8] {
        self.out.paps.as_slice()
    }
    fn owners(&self) -> &[SlotOwner] {
        self.out.owners.as_slice()
    }
}

/// A deliberately naive, obviously-correct reference merger.
///
/// For every mutation it recomputes all slots with a plain slot-serial scan
/// (slots in the outer loop, sources in the inner), the most direct possible
/// statement of the merge rule. It shares the merger's [`PropCaps`] storage so
/// the differential test compares two implementations over the same backing.
#[derive(Debug)]
struct Naive {
    table: SourceTable<PropCaps>,
    out: OutBuf,
}

impl Naive {
    /// The priority this source contributes at `slot`: its stored effective
    /// `pap`, but `0` for slots beyond its valid level count.
    fn effective_pap(src: &MergeSourceEntry, slot: usize) -> u8 {
        if slot < src.valid_level_count {
            src.pap[slot]
        } else {
            0
        }
    }

    fn remerge(&mut self) {
        for slot in 0..MAX_SLOTS {
            let mut best_level: u8 = 0;
            let mut best_pap: u8 = 0;
            let mut best_owner = SlotOwner::NONE;

            for src in self.table.sources() {
                let pap = Self::effective_pap(src, slot);
                let level = src.levels[slot];
                let take = pap > best_pap || (pap == best_pap && level > best_level && pap != 0);
                if take {
                    best_level = level;
                    best_pap = pap;
                    best_owner = SlotOwner(src.index);
                }
            }

            self.out.levels[slot] = best_level;
            self.out.paps[slot] = best_pap;
            self.out.owners[slot] = best_owner;
        }
    }
}

impl TestMerger for Naive {
    fn create() -> Self {
        Self {
            table: SourceTable::new(),
            out: OutBuf::empty(),
        }
    }

    fn add_source(&mut self) -> SourceId {
        self.table.add().unwrap()
    }

    fn remove_source(&mut self, id: SourceId) {
        if let Some(i) = self.table.resolve(id) {
            self.table.remove_at(i);
            self.remerge();
        }
    }

    fn update_levels(&mut self, id: SourceId, levels: &[u8]) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let n = levels.len().min(MAX_SLOTS);
        let s = self.table.source_mut(i);
        let old = s.valid_level_count;
        s.levels[..n].copy_from_slice(&levels[..n]);
        if old > n {
            s.levels[n..old].fill(0);
        }
        s.valid_level_count = n;
        self.remerge();
    }

    fn update_pap(&mut self, id: SourceId, pap: &[u8]) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let n = pap.len().min(MAX_SLOTS);
        let s = self.table.source_mut(i);
        s.using_universe_priority = false;
        s.pap[..n].copy_from_slice(&pap[..n]);
        s.pap[n..].fill(0);
        s.pap_count = n;
        self.remerge();
    }

    fn update_universe_priority(&mut self, id: SourceId, priority: Priority) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let priority = priority.get();
        let s = self.table.source_mut(i);
        s.universe_priority = priority;
        if s.using_universe_priority {
            s.pap.fill(universe_priority_to_pap(priority));
            s.pap_count = MAX_SLOTS;
        }
        self.remerge();
    }

    fn remove_pap(&mut self, id: SourceId) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let s = self.table.source_mut(i);
        s.using_universe_priority = true;
        s.pap.fill(universe_priority_to_pap(s.universe_priority));
        s.pap_count = MAX_SLOTS;
        self.remerge();
    }

    fn levels(&self) -> &[u8] {
        self.out.levels.as_slice()
    }
    fn priorities(&self) -> &[u8] {
        self.out.paps.as_slice()
    }
    fn owners(&self) -> &[SlotOwner] {
        self.out.owners.as_slice()
    }
}

// --- equivalence helpers -----------------------------------------------------

/// Asserts the structural owner invariants on one merger's output.
fn check_owner_invariants<M: TestMerger>(m: &M) {
    let (priorities, levels, owners) = (m.priorities(), m.levels(), m.owners());
    for slot in 0..MAX_SLOTS {
        if priorities[slot] == 0 {
            assert!(owners[slot].is_none(), "unsourced slot {slot} has an owner");
            assert_eq!(
                levels[slot], 0,
                "unsourced slot {slot} has a non-zero level"
            );
        } else {
            assert!(owners[slot].is_some(), "sourced slot {slot} has no owner");
        }
    }
}

/// Asserts two mergers produced equivalent output (levels, priorities, owners,
/// and the structural owner invariants on each).
fn assert_equivalent<A: TestMerger, B: TestMerger>(a: &A, b: &B, context: &str) {
    assert_eq!(a.levels(), b.levels(), "levels diverged: {context}");
    assert_eq!(
        a.priorities(),
        b.priorities(),
        "priorities diverged: {context}"
    );
    check_owner_invariants(a);
    check_owner_invariants(b);
    assert_eq!(a.owners(), b.owners(), "owners diverged: {context}");
}

/// One update applied to a merger.
///
/// Source-referencing ops carry an opaque selector that is reduced modulo the
/// number of sources ever added (see [`apply_op`]), so they address a source
/// without the strategy needing to know how many exist at that point in the
/// sequence. `AddSource`/`RemoveSource` exercise the source lifecycle and index
/// reuse that the fixed up-front source set would otherwise never reach.
#[derive(Debug, Clone)]
enum Op {
    Levels(usize, Vec<u8>),
    Pap(usize, Vec<u8>),
    UniversePriority(usize, Priority),
    RemovePap(usize),
    AddSource,
    RemoveSource(usize),
}

/// Applies `op`, growing `ids` on `AddSource`. Removed sources are left in
/// `ids` (now stale) on purpose: later ops addressing them exercise the
/// ignore-stale-handle path, and a reused index makes the stale handle alias
/// the new source. `ids` is never empty (the scenario starts with >= 1 source
/// and entries are never dropped), so the modulo is always well-defined.
fn apply_op<M: TestMerger>(m: &mut M, ids: &mut Vec<SourceId>, op: &Op) {
    match op {
        Op::Levels(i, levels) => m.update_levels(ids[*i % ids.len()], levels),
        Op::Pap(i, pap) => m.update_pap(ids[*i % ids.len()], pap),
        Op::UniversePriority(i, p) => m.update_universe_priority(ids[*i % ids.len()], *p),
        Op::RemovePap(i) => m.remove_pap(ids[*i % ids.len()]),
        Op::RemoveSource(i) => m.remove_source(ids[*i % ids.len()]),
        Op::AddSource => ids.push(m.add_source()),
    }
}

/// Drives the same op sequence through the shipped merger and the reference,
/// asserting equivalence after every step.
///
/// Both id lists grow in lockstep on `AddSource`, so they stay equal in length
/// and the modulo selection picks the same source on each side.
fn differential(num: usize, ops: &[Op]) {
    let mut shipped = PropMerger::create();
    let mut reference = Naive::create();
    let mut shipped_ids: Vec<_> = (0..num).map(|_| shipped.add_source().unwrap()).collect();
    let mut reference_ids: Vec<_> = (0..num).map(|_| reference.add_source()).collect();

    for (step, op) in ops.iter().enumerate() {
        apply_op(&mut shipped, &mut shipped_ids, op);
        apply_op(&mut reference, &mut reference_ids, op);
        let ctx = alloc::format!("after step {step}: {op:?}");
        assert_equivalent(&shipped, &reference, &ctx);
    }
}

// --- differential property test ----------------------------------------------

fn op_strategy() -> impl Strategy<Value = Op> {
    // The source selector ranges over a fixed space and is reduced modulo the
    // live id count in `apply_op`, so it need not track the current count.
    prop_oneof![
        (0usize..64, vec(any::<u8>(), 1..=64)).prop_map(|(i, l)| Op::Levels(i, l)),
        (0usize..64, vec(any::<u8>(), 1..=64)).prop_map(|(i, p)| Op::Pap(i, p)),
        (0usize..64, 0u8..=Priority::MAX)
            .prop_map(|(i, p)| Op::UniversePriority(i, Priority::new(p).unwrap())),
        (0usize..64).prop_map(Op::RemovePap),
        Just(Op::AddSource),
        (0usize..64).prop_map(Op::RemoveSource),
    ]
}

fn scenario_strategy() -> impl Strategy<Value = (usize, Vec<Op>)> {
    (1usize..=4).prop_flat_map(|num| (Just(num), vec(op_strategy(), 1..=80)))
}

proptest! {
    #[test]
    fn dmx_merger_matches_naive((num, ops) in scenario_strategy()) {
        differential(num, &ops);
    }
}
