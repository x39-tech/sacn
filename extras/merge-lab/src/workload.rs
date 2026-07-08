// Deterministic workload generation for the merger benchmarks.
//
// The dimensions we play with in the benchmark workloads:
//
// - source count - 1-2 sources is the most common case, but higher counts
//   exist in large installations.
// - priority structure (`PriorityStructure`) - how much the sources contend
//   for ownership of each slot.
// - update pattern (`UpdatePattern`) - how each successive frame differs from
//   the last, including the pathological owner-decrease "chase" that forces
//   the incremental algorithm into its rescan path.
//
// A `Scenario` picks one point in that space; `Scenario::generate` turns it
// into a `Workload`: a `setup` phase applied once, then a `frames` phase that
// is the thing actually measured. Generation is seeded and fully deterministic,
// so a run is reproducible and the same frames feed every candidate.

use crate::{Merger, MAX_SLOTS};

/// How sources are prioritized relative to each other - i.e. how much they
/// contend for ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityStructure {
    /// Every source uses the same universe priority, so all compete at every
    /// slot. Maximum owner contention - the stress case for HTP tie-breaking.
    EqualUniverse(u8),
    /// Sources get distinct, ascending universe priorities, so the top source
    /// dominates and ownership rarely moves.
    DistinctUniverse,
    /// Each source sends explicit PAP covering a disjoint band of slots, so the
    /// sources barely contend - a common real topology.
    PartialPap,
}

/// How each frame's levels differ from the previous frame's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePattern {
    /// A handful of slots change per frame. Realistic console output.
    SmallDelta,
    /// All 512 slots change every frame. The heavy-churn case.
    FullChurn,
    /// One source repeatedly seizes then releases ownership of every slot by
    /// toggling its *level*, forcing the incremental algorithm's level-path
    /// owner-decrease rescan (in `update_levels_multi`) each release.
    LevelChase,
    /// One source repeatedly seizes then releases ownership of every slot by
    /// toggling its *priority*, forcing the incremental algorithm's
    /// priority-path owner-decrease rescan (in `merge_new_priorities`) each
    /// release.
    PriorityChase,
}

/// One point in the workload space.
#[derive(Debug, Clone, Copy)]
pub struct Scenario {
    /// Number of sources in the merger.
    pub sources: usize,
    /// How the sources are prioritized.
    pub priority: PriorityStructure,
    /// How frames evolve.
    pub pattern: UpdatePattern,
    /// Number of measured frames (one source update each).
    pub frames: usize,
    /// Seed for deterministic generation.
    pub seed: u64,
}

/// One update applied to the merger, referring to a source by its position in
/// the order the workload added them (not the merger's own handle).
#[derive(Debug, Clone)]
pub enum Op {
    /// Set a source's levels.
    Levels(usize, Vec<u8>),
    /// Set a source's per-address priorities.
    Pap(usize, Vec<u8>),
    /// Set a source's universe priority.
    UniversePriority(usize, u8),
    /// Remove a source's PAP.
    RemovePap(usize),
}

/// A generated workload: a one-time `setup` followed by measured `frames`.
#[derive(Debug, Clone)]
pub struct Workload {
    /// Number of sources to add before applying any ops.
    pub sources: usize,
    /// Applied once, before measurement, to reach steady state.
    pub setup: Vec<Op>,
    /// The measured sequence, replayed by the benchmark.
    pub frames: Vec<Op>,
}

/// Applies one [`Op`] to a merger, mapping the op's source index through `ids`.
pub fn apply_op<M: Merger>(merger: &mut M, ids: &[M::Handle], op: &Op) {
    match op {
        Op::Levels(i, levels) => merger.update_levels(ids[*i], levels),
        Op::Pap(i, pap) => merger.update_pap(ids[*i], pap),
        Op::UniversePriority(i, p) => merger.update_universe_priority(ids[*i], *p),
        Op::RemovePap(i) => merger.remove_pap(ids[*i]),
    }
}

impl Scenario {
    /// Generates the deterministic [`Workload`] for this scenario.
    pub fn generate(&self) -> Workload {
        let mut rng = fastrand::Rng::with_seed(self.seed);
        let n = self.sources;

        // Per-source running level state, so SmallDelta can mutate the previous
        // frame and the chase patterns can hold non-target sources steady.
        let mut levels: Vec<[u8; MAX_SLOTS]> = vec![[0; MAX_SLOTS]; n];

        let mut setup = Vec::new();

        // Priority setup.
        match self.priority {
            PriorityStructure::EqualUniverse(p) => {
                for i in 0..n {
                    setup.push(Op::UniversePriority(i, p));
                }
            }
            PriorityStructure::DistinctUniverse => {
                for i in 0..n {
                    // Distinct and within the valid universe-priority range.
                    let p = (100 + i).min(200) as u8;
                    setup.push(Op::UniversePriority(i, p));
                }
            }
            PriorityStructure::PartialPap => {
                let band = MAX_SLOTS.div_ceil(n.max(1));
                for i in 0..n {
                    let mut pap = vec![0u8; MAX_SLOTS];
                    let start = i * band;
                    let end = (start + band).min(MAX_SLOTS);
                    for slot in start..end {
                        pap[slot] = 100;
                    }
                    setup.push(Op::Pap(i, pap));
                }
            }
        }

        // Initial levels: a full frame per source, establishing steady state.
        for i in 0..n {
            for slot in 0..MAX_SLOTS {
                levels[i][slot] = rng.u8(..);
            }
            setup.push(Op::Levels(i, levels[i].to_vec()));
        }

        // Measured frames.
        let mut frames = Vec::with_capacity(self.frames);
        match self.pattern {
            UpdatePattern::SmallDelta => {
                for f in 0..self.frames {
                    let i = f % n;
                    // Mutate a few slots of this source's current levels.
                    for _ in 0..8 {
                        let slot = rng.usize(..MAX_SLOTS);
                        levels[i][slot] = rng.u8(..);
                    }
                    frames.push(Op::Levels(i, levels[i].to_vec()));
                }
            }
            UpdatePattern::FullChurn => {
                for f in 0..self.frames {
                    let i = f % n;
                    for slot in 0..MAX_SLOTS {
                        levels[i][slot] = rng.u8(..);
                    }
                    frames.push(Op::Levels(i, levels[i].to_vec()));
                }
            }
            UpdatePattern::LevelChase => {
                // The highest-id source alternately seizes every slot (all 255)
                // and releases it (all 0); the others hold a steady mid level so
                // they are the fallback owners the release rescans must find.
                let target = n - 1;
                for i in 0..n {
                    if i != target {
                        setup.push(Op::Levels(i, vec![128; MAX_SLOTS]));
                    }
                }
                for f in 0..self.frames {
                    let buf = if f % 2 == 0 {
                        [255; MAX_SLOTS]
                    } else {
                        [0; MAX_SLOTS]
                    };
                    frames.push(Op::Levels(target, buf.to_vec()));
                }
            }
            UpdatePattern::PriorityChase => {
                // The highest-id source alternately seizes every slot and
                // releases it via per-address priority. The others hold a shared
                // background priority.
                //
                // Only coherent under EqualUniverse (the bench restricts it so),
                // whose shared priority we bracket the toggle around.
                let PriorityStructure::EqualUniverse(base) = self.priority else {
                    unreachable!("bench restricts PriorityChase to EqualUniverse");
                };
                let hi = base.saturating_add(50);
                let lo = base.saturating_sub(50).max(1);
                let target = n - 1;
                for i in 0..n {
                    let lvl = if i == target { 200 } else { 128 };
                    setup.push(Op::Levels(i, vec![lvl; MAX_SLOTS]));
                }
                for f in 0..self.frames {
                    let up = if f % 2 == 0 { hi } else { lo };
                    frames.push(Op::UniversePriority(target, up));
                }
            }
        }

        Workload {
            sources: n,
            setup,
            frames,
        }
    }
}

/// Builds a merger of type `M`, adds the workload's sources, and applies its
/// setup phase, returning the merger and the assigned source ids.
///
/// The returned merger is at steady state, ready for the measured frames.
pub fn prepare<M: Merger>(workload: &Workload) -> (M, Vec<M::Handle>) {
    let mut merger = M::create();
    let ids: Vec<M::Handle> = (0..workload.sources).map(|_| merger.add_source()).collect();
    for op in &workload.setup {
        apply_op(&mut merger, &ids, op);
    }
    (merger, ids)
}
