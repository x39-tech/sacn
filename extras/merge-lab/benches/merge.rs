// Criterion benchmarks comparing the merger candidates.
//
// Each benchmark replays a generated Workload's measured `frames` through a
// candidate that has already been brought to steady state. The measurement
// choices that make the numbers trustworthy:
//
// - Per-batch fresh state (`iter_batched`): the merger is re-prepared (sources
//   added, setup applied) outside the timed section, so we measure only the
//   steady-state frame updates, and every timed run starts from the same state
//   - important for the chase patterns, whose effect depends on the prior frame.
// - Changing frames: the generator emits distinct data each frame, so we never
//   accidentally measure a "nothing changed" early-out instead of a merge.
// - `black_box` on the output: the per-frame results feed forward and the final
//   output is consumed through `black_box`, so the optimizer can't elide the
//   merge.

use std::hint::black_box;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{
    criterion_group, criterion_main, BatchSize, BenchmarkGroup, BenchmarkId, Criterion,
};

use merge_lab::workload::{
    apply_op, prepare, PriorityStructure, Scenario, UpdatePattern, Workload,
};
use merge_lab::{FullRemerge, Incremental, Merger, SacnMerger};

/// Measured frames per workload (one source update each).
const FRAMES: usize = 256;
/// Fixed seed so every run benchmarks the identical input.
const SEED: u64 = 0x5AC2;
/// Generally you won't find double-digit numbers of sources for a universe.
const SOURCE_COUNTS: &[usize] = &[1, 2, 4, 8, 16];

fn priority_label(p: &PriorityStructure) -> &'static str {
    match p {
        PriorityStructure::EqualUniverse(_) => "equal",
        PriorityStructure::DistinctUniverse => "distinct",
        PriorityStructure::PartialPap => "partial_pap",
    }
}

/// Benchmarks one candidate (the comparison function `name`) at one source
/// count (the numeric parameter `n`) over one workload's frames.
fn bench_candidate<M: Merger>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    n: usize,
    w: &Workload,
) {
    group.bench_with_input(BenchmarkId::new(name, n), w, |b, w| {
        b.iter_batched(
            || prepare::<M>(w),
            |(mut merger, ids)| {
                for op in &w.frames {
                    apply_op(&mut merger, &ids, op);
                }
                // Consume the result so the whole frame chain is observable.
                let out = merger.output();
                black_box(out.levels.iter().fold(0u32, |acc, &x| acc + x as u32))
            },
            BatchSize::SmallInput,
        );
    });
}

/// Benchmarks every candidate across the source-count range for one update
/// pattern, one group per priority structure so each group yields a clean
/// candidate-vs-source-count comparison.
fn bench_pattern(c: &mut Criterion, pattern: UpdatePattern, pattern_name: &str) {
    let priorities = [
        PriorityStructure::EqualUniverse(100),
        PriorityStructure::DistinctUniverse,
        PriorityStructure::PartialPap,
    ];

    for priority in priorities {
        // LevelChase is only meaningful under equal priority, and
        // PriorityChase is redundant with any PriorityStructure, so we run
        // them both only paired with EqualUniverse.
        let is_chase = matches!(
            pattern,
            UpdatePattern::LevelChase | UpdatePattern::PriorityChase
        );
        if is_chase && !matches!(priority, PriorityStructure::EqualUniverse(_)) {
            continue;
        }

        let mut group = c.benchmark_group(format!("{pattern_name}/{}", priority_label(&priority)));
        // Keep total runtime sane across the large matrix; still plenty of
        // samples for stable nanosecond-scale measurements.
        group
            .sample_size(50)
            .warm_up_time(Duration::from_millis(500))
            .measurement_time(Duration::from_secs(2));

        for &n in SOURCE_COUNTS {
            let w = Scenario {
                sources: n,
                priority,
                pattern,
                frames: FRAMES,
                seed: SEED,
            }
            .generate();

            bench_candidate::<Incremental>(&mut group, "incremental", n, &w);
            bench_candidate::<FullRemerge>(&mut group, "full_remerge", n, &w);
            bench_candidate::<SacnMerger>(&mut group, "sacn", n, &w);
            #[cfg(all(feature = "simd", any(target_arch = "aarch64", target_arch = "x86_64")))]
            bench_candidate::<merge_lab::SimdRemerge>(&mut group, "simd_remerge", n, &w);
            #[cfg(feature = "etc")]
            bench_candidate::<merge_lab::EtcMerger>(&mut group, "etc", n, &w);
        }

        group.finish();
    }
}

fn benches(c: &mut Criterion) {
    bench_pattern(c, UpdatePattern::SmallDelta, "small_delta");
    bench_pattern(c, UpdatePattern::FullChurn, "full_churn");
    bench_pattern(c, UpdatePattern::LevelChase, "level_chase");
    bench_pattern(c, UpdatePattern::PriorityChase, "priority_chase");
}

criterion_group!(merge, benches);
criterion_main!(merge);
