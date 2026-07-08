// Correctness oracle for the merger candidates.
//
// Includes unit tests against hand-computed HTP/PAP results, as well as a
// differential property test that drives random operation sequences through
// all candidates and asserts that they stay equivalent after every step.
//
// [`Naive`] is the single source of truth: an obviously-correct slot-serial
// reference that every other candidate is compared against. Equivalence is
// defined as: identical `levels` and `paps` for every slot, plus the structural
// owner invariants (a slot has an owner exactly when its priority is non-zero,
// and an unsourced slot has level 0).
//
// Owner *identity* in a priority+level tie depends on which tie-break family a
// candidate belongs to. There are two, each internally deterministic:
//
//   - State-based (`Naive`, `FullRemerge`, `SimdRemerge`): a slot's owner is a
//     pure function of the current state - the lowest concurrent source index
//     among the sources maximal on (priority, level). These re-merge from
//     scratch in ascending index order with strictly-greater comparisons, so
//     they all agree exactly, owners included.
//   - Incumbency-based (`Incremental`, `EtcMerger`): a slot's owner is
//     path-dependent - the incumbent keeps a slot until a challenger strictly
//     exceeds it, so the first source to reach the winning (priority, level)
//     holds the slot. `Incremental` is a port of ETC's algorithm and matches it.
//
// The differential tests therefore assert exact owner equality only within a
// family (`exact_owners`), and the structural owner invariants across families.
// The pinned tie-rule unit tests below document the exact owner each family
// selects.

use merge_lab::workload::{apply_op, Op, PriorityStructure, Scenario, UpdatePattern, Workload};
use merge_lab::{
    universe_priority_to_pap, FullRemerge, Incremental, Merger, MergerHandle, OutBuf, Output,
    SacnMerger, SourceIndex, SourceTable, MAX_SLOTS, NO_OWNER,
};

use proptest::collection::vec;
use proptest::prelude::*;

/// A deliberately naive, obviously-correct reference merger.
///
/// State-based tie-break family.
#[derive(Debug)]
struct Naive {
    table: SourceTable,
    out: OutBuf,
}

impl Naive {
    fn remerge(&mut self) {
        for slot in 0..MAX_SLOTS {
            let mut best_level: u8 = 0;
            let mut best_pap: u8 = 0;
            let mut best_owner = NO_OWNER;

            for src in self.table.sources() {
                let pap = src.effective_pap(slot);
                let level = src.levels[slot];
                let take = pap > best_pap || (pap == best_pap && level > best_level && pap != 0);
                if take {
                    best_level = level;
                    best_pap = pap;
                    best_owner = src.index;
                }
            }

            self.out.levels[slot] = best_level;
            self.out.paps[slot] = best_pap;
            self.out.owners[slot] = best_owner;
        }
    }
}

impl Merger for Naive {
    type Handle = SourceIndex;

    fn create() -> Self {
        Self {
            table: SourceTable::new(),
            out: OutBuf::empty(),
        }
    }

    fn add_source(&mut self) -> SourceIndex {
        self.table.add()
    }

    fn remove_source(&mut self, id: SourceIndex) {
        if let Some(i) = self.table.resolve(id) {
            self.table.remove_at(i);
            self.remerge();
        }
    }

    fn update_levels(&mut self, id: SourceIndex, levels: &[u8]) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let n = levels.len();
        let s = &mut self.table.sources_mut()[i];
        let old = s.valid_level_count;
        s.levels[..n].copy_from_slice(levels);
        if old > n {
            s.levels[n..old].fill(0);
        }
        s.valid_level_count = n;
        self.remerge();
    }

    fn update_pap(&mut self, id: SourceIndex, pap: &[u8]) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let n = pap.len();
        let s = &mut self.table.sources_mut()[i];
        s.using_universe_priority = false;
        s.pap[..n].copy_from_slice(pap);
        s.pap[n..].fill(0);
        s.pap_count = n;
        self.remerge();
    }

    fn update_universe_priority(&mut self, id: SourceIndex, priority: u8) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let s = &mut self.table.sources_mut()[i];
        s.universe_priority = priority;
        s.universe_priority_uninitialized = false;
        if s.using_universe_priority {
            s.pap.fill(universe_priority_to_pap(priority));
            s.pap_count = MAX_SLOTS;
        }
        self.remerge();
    }

    fn remove_pap(&mut self, id: SourceIndex) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let s = &mut self.table.sources_mut()[i];
        s.using_universe_priority = true;
        s.pap.fill(universe_priority_to_pap(s.universe_priority));
        s.pap_count = MAX_SLOTS;
        self.remerge();
    }

    fn output(&self) -> Output<'_> {
        self.out.as_output()
    }
}

// --- equivalence helpers -----------------------------------------------------

/// Asserts the per-impl owner invariants.
fn check_owner_invariants(out: &Output) {
    for slot in 0..MAX_SLOTS {
        if out.paps[slot] == 0 {
            assert_eq!(
                out.owners[slot], NO_OWNER,
                "unsourced slot {slot} has an owner"
            );
            assert_eq!(
                out.levels[slot], 0,
                "unsourced slot {slot} has a non-zero level"
            );
        } else {
            assert_ne!(
                out.owners[slot], NO_OWNER,
                "sourced slot {slot} has no owner"
            );
        }
    }
}

/// Asserts two candidates produced equivalent output.
///
/// Always checks `levels`, `paps`, and the structural owner invariants. When
/// `exact_owners` is set, also requires owner identity to match exactly - use
/// this only for two members of the same tie-break family (see the module docs).
fn assert_equivalent(a: &Output, b: &Output, exact_owners: bool, context: &str) {
    assert_eq!(a.levels, b.levels, "levels diverged: {context}");
    assert_eq!(a.paps, b.paps, "paps diverged: {context}");
    check_owner_invariants(a);
    check_owner_invariants(b);
    if exact_owners {
        assert_eq!(a.owners, b.owners, "owners diverged: {context}");
    }
}

/// Drives the same op sequence through two mergers, asserting equivalence after
/// every step.
fn differential<A: Merger, B: Merger>(num: usize, ops: &[Op], exact_owners: bool) {
    let mut a = A::create();
    let mut b = B::create();
    let a_ids: Vec<_> = (0..num).map(|_| a.add_source()).collect();
    let b_ids: Vec<_> = (0..num).map(|_| b.add_source()).collect();

    for (step, op) in ops.iter().enumerate() {
        apply_op(&mut a, &a_ids, op);
        apply_op(&mut b, &b_ids, op);
        let ctx = format!("after step {step}: {op:?}");
        assert_equivalent(&a.output(), &b.output(), exact_owners, &ctx);
    }
}

/// Replays a whole generated workload through `M` and the [`Naive`] reference,
/// asserting equivalence at the end.
fn workload_matches_naive<M: Merger>(w: &Workload, exact_owners: bool, ctx: &str) {
    let mut m = M::create();
    let mut naive = Naive::create();
    let m_ids: Vec<_> = (0..w.sources).map(|_| m.add_source()).collect();
    let naive_ids: Vec<_> = (0..w.sources).map(|_| naive.add_source()).collect();
    for op in w.setup.iter().chain(w.frames.iter()) {
        apply_op(&mut m, &m_ids, op);
        apply_op(&mut naive, &naive_ids, op);
    }
    assert_equivalent(&m.output(), &naive.output(), exact_owners, ctx);
}

/// These are just kinda random
const ORACLE_SEEDS: [u64; 4] = [0xDA7A, 0x1234_5678, 0x0000_ACE1, 0xFFFF_0001];

/// Invokes `check` for every (seed x sources x priority x pattern) workload.
fn for_each_workload(mut check: impl FnMut(&Workload, &str)) {
    let sources = [1usize, 2, 4, 8];
    let priorities = [
        PriorityStructure::EqualUniverse(100),
        PriorityStructure::DistinctUniverse,
        PriorityStructure::PartialPap,
    ];
    let patterns = [
        UpdatePattern::SmallDelta,
        UpdatePattern::FullChurn,
        UpdatePattern::LevelChase,
        UpdatePattern::PriorityChase,
    ];

    for &seed in &ORACLE_SEEDS {
        for &n in &sources {
            for &priority in &priorities {
                for &pattern in &patterns {
                    // The chase patterns are only coherent (and only generated)
                    // under equal universe priority; mirror that restriction.
                    let is_chase = matches!(
                        pattern,
                        UpdatePattern::LevelChase | UpdatePattern::PriorityChase
                    );
                    if is_chase && !matches!(priority, PriorityStructure::EqualUniverse(_)) {
                        continue;
                    }
                    let w = Scenario {
                        sources: n,
                        priority,
                        pattern,
                        frames: 50,
                        seed,
                    }
                    .generate();
                    let ctx = format!("seed {seed:#x}, {n} sources, {priority:?}, {pattern:?}");
                    check(&w, &ctx);
                }
            }
        }
    }
}

/// Runs the canonical priority+level tie and returns `(owner, lower, higher)`:
/// the resulting owner of slot 0, plus the lower- and higher-index source ids.
///
/// `b` (the higher index) seizes the slot first, then `a` (the lower index) ties
/// it exactly on priority and level. The two families resolve this differently:
/// the state-based family hands it to `a` (lowest index), the incumbency-based
/// family keeps it with `b` (the incumbent).
fn tie_owner<M: Merger>() -> (SourceIndex, SourceIndex, SourceIndex) {
    let mut m = M::create();
    let a = m.add_source();
    let b = m.add_source();
    m.update_universe_priority(a, 100);
    m.update_universe_priority(b, 100);
    m.update_levels(b, &[100]); // b (higher index) seizes the slot first
    m.update_levels(a, &[100]); // a (lower index) ties it exactly

    let out = m.output();
    assert_eq!(out.levels[0], 100, "tie setup should leave level 100");
    assert_eq!(out.paps[0], 100, "tie setup should leave priority 100");
    (out.owners[0], a.index(), b.index())
}

// --- unit tests --------------------------------------------------------------

#[test]
fn htp_among_equal_priority() {
    let mut m = FullRemerge::create();
    let a = m.add_source();
    let b = m.add_source();
    m.update_universe_priority(a, 100);
    m.update_universe_priority(b, 100);

    m.update_levels(a, &[100, 100, 100]);
    m.update_levels(b, &[50, 200, 100]);

    let out = m.output();
    // Slot 0: A higher. Slot 1: B higher. Slot 2: tie -> lower id (A) wins.
    assert_eq!(&out.levels[..3], &[100, 200, 100]);
    assert_eq!(&out.paps[..3], &[100, 100, 100]);
    assert_eq!(&out.owners[..3], &[a.index(), b.index(), a.index()]);
}

#[test]
fn higher_priority_beats_higher_level() {
    let mut m = FullRemerge::create();
    let a = m.add_source();
    let b = m.add_source();
    m.update_universe_priority(a, 150);
    m.update_universe_priority(b, 100);

    m.update_levels(a, &[10]);
    m.update_levels(b, &[255]);

    let out = m.output();
    // A wins despite a far lower level, because its priority is higher.
    assert_eq!(out.levels[0], 10);
    assert_eq!(out.paps[0], 150);
    assert_eq!(out.owners[0], a.index());
}

#[test]
fn owner_release_falls_back_to_next_source() {
    let mut m = FullRemerge::create();
    let a = m.add_source();
    let b = m.add_source();
    m.update_universe_priority(a, 100);
    m.update_universe_priority(b, 100);

    m.update_levels(a, &[100]);
    m.update_levels(b, &[200]); // B owns at 200
    assert_eq!(m.output().owners[0], b.index());

    m.update_levels(b, &[50]); // B drops below A: A reclaims
    let out = m.output();
    assert_eq!(out.levels[0], 100);
    assert_eq!(out.owners[0], a.index());
}

#[test]
fn universe_priority_zero_becomes_pap_one() {
    let mut m = FullRemerge::create();
    let a = m.add_source();
    m.update_universe_priority(a, 0);
    m.update_levels(a, &[42]);

    let out = m.output();
    assert_eq!(out.paps[0], 1, "universe priority 0 must convert to PAP 1");
    assert_eq!(out.owners[0], a.index());
    assert_eq!(out.levels[0], 42);
}

// --- pinned tie-break contract -----------------------------------------------

#[test]
fn state_family_breaks_tie_toward_lowest_index() {
    for (owner, lower, _higher) in [
        tie_owner::<Naive>(),
        tie_owner::<FullRemerge>(),
        tie_owner::<SacnMerger>(),
    ] {
        assert_eq!(
            owner, lower,
            "state-based merge must hand a priority+level tie to the lowest index"
        );
    }
}

#[test]
fn incumbency_family_keeps_tie_with_incumbent() {
    let (owner, _lower, higher) = tie_owner::<Incremental>();
    assert_eq!(
        owner, higher,
        "incremental must keep a priority+level tie with the incumbent owner"
    );
}

// --- differential property tests ---------------------------------------------

fn op_strategy(num: usize) -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..num, vec(any::<u8>(), 1..=64)).prop_map(|(i, l)| Op::Levels(i, l)),
        (0..num, vec(any::<u8>(), 1..=64)).prop_map(|(i, p)| Op::Pap(i, p)),
        (0..num, 0u8..=200).prop_map(|(i, p)| Op::UniversePriority(i, p)),
        (0..num).prop_map(Op::RemovePap),
    ]
}

fn scenario_strategy() -> impl Strategy<Value = (usize, Vec<Op>)> {
    (1usize..=4).prop_flat_map(|num| (Just(num), vec(op_strategy(num), 1..=80)))
}

proptest! {
    // Same tie-break family
    #[test]
    fn full_remerge_matches_naive((num, ops) in scenario_strategy()) {
        differential::<FullRemerge, Naive>(num, &ops, true);
    }

    // Different tie-break family
    #[test]
    fn incremental_matches_naive((num, ops) in scenario_strategy()) {
        differential::<Incremental, Naive>(num, &ops, false);
    }

    // Same tie-break family
    #[test]
    fn sacn_matches_naive((num, ops) in scenario_strategy()) {
        differential::<SacnMerger, Naive>(num, &ops, true);
    }
}

// --- the generated workloads, through every available impl --------------------

#[test]
fn generated_workloads_agree() {
    for_each_workload(|w, ctx| {
        workload_matches_naive::<FullRemerge>(w, true, ctx);
        workload_matches_naive::<Incremental>(w, false, ctx);
        workload_matches_naive::<SacnMerger>(w, true, ctx);
    });
}

// --- the SIMD candidate, against the reference -------------------------------

#[cfg(all(feature = "simd", any(target_arch = "aarch64", target_arch = "x86_64")))]
mod simd {
    use super::*;
    use merge_lab::SimdRemerge;

    proptest! {
        /// `SimdRemerge` is in the state-based family, so owners must match the
        /// reference exactly.
        #[test]
        fn simd_matches_naive((num, ops) in scenario_strategy()) {
            differential::<SimdRemerge, Naive>(num, &ops, true);
        }
    }

    #[test]
    fn simd_matches_naive_on_workloads() {
        for_each_workload(|w, ctx| workload_matches_naive::<SimdRemerge>(w, true, ctx));
    }

    #[test]
    fn simd_breaks_tie_toward_lowest_index() {
        let (owner, lower, _higher) = tie_owner::<SimdRemerge>();
        assert_eq!(
            owner, lower,
            "SIMD merge must hand a tie to the lowest index"
        );
    }
}

// --- validation against the reference ETC C library --------------------------
//
// ETC is in the incumbency-based family, so owner identity is checked only
// structurally, not exactly.
#[cfg(feature = "etc")]
mod etc {
    use super::*;
    use merge_lab::EtcMerger;

    fn op_strategy_no_removepap(num: usize) -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..num, vec(any::<u8>(), 1..=64)).prop_map(|(i, l)| Op::Levels(i, l)),
            (0..num, vec(any::<u8>(), 1..=64)).prop_map(|(i, p)| Op::Pap(i, p)),
            (0..num, 0u8..=200).prop_map(|(i, p)| Op::UniversePriority(i, p)),
            (0..num).prop_map(Op::RemovePap),
        ]
    }

    fn no_removepap_sequence() -> impl Strategy<Value = (usize, Vec<Op>)> {
        (1usize..=4).prop_flat_map(|num| (Just(num), vec(op_strategy_no_removepap(num), 1..=80)))
    }

    proptest! {
        #[test]
        fn etc_matches_naive((num, ops) in no_removepap_sequence()) {
            differential::<EtcMerger, Naive>(num, &ops, false);
        }
    }

    #[test]
    fn etc_matches_naive_on_workloads() {
        // The generated workloads never emit `RemovePap`, so they are safe here.
        for_each_workload(|w, ctx| workload_matches_naive::<EtcMerger>(w, false, ctx));
    }

    #[test]
    fn etc_keeps_tie_with_incumbent() {
        let (owner, _lower, higher) = tie_owner::<EtcMerger>();
        assert_eq!(
            owner, higher,
            "ETC must keep a priority+level tie with the incumbent owner"
        );
    }
}
