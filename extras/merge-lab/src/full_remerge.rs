// Each mutation causes a full re-merge (non-SIMD version).
//
// Every mutating call updates the affected source's stored state and then
// recomputes all [`MAX_SLOTS`] outputs from scratch. The remerge() function
// is written as a branch-free pass over all sources and slots (sources in
// the outer loop, slots in the inner) The algorithm is essentially the same
// as the hand-written SIMD implementations.
//
// The loop has been observed to be autovectorized by LLVM under some
// circumstances such that the performance approaches (but does not fully
// reach) that of the hand-written SIMD implementations.
//
// It is `O(slots x sources)` on *every* update regardless of what changed.
//
// Verifying the vectorization:
//
// The vectorization above is an LLVM cost-model decision, not an explicit
// intrinsic, so it can silently regress to scalar code on a different compiler
// version, a different `-C target-cpu`, or an innocent-looking edit to this loop.
// You can check whether the code was autovectorized:
//
// 1. Build with the vector unit enabled (without it there is nothing to
//    vectorize to, and the lab's numbers assume it):
//
//        RUSTFLAGS="-C target-cpu=native" cargo build --release -p merge-lab
//
// 2. Disassemble and look for the vector markers. remerge() is inlined into the
//    Merger trait methods, so there is no `remerge` symbol; inspect the hot path,
//    update_levels (the target dir is the workspace root):
//
//        objdump -d --demangle \
//          "$(ls -t target/release/deps/libmerge_lab-*.rlib | head -1)" \
//          | grep -A300 'FullRemerge.*::update_levels>:' \
//          | grep -E 'vpblendvb|vpmovsxbw|bsl|\bbit\b|ushll'
//
//    Vectorized if that prints anything. The pattern is the same on both
//    architectures: SIMD compares, a blend, and the byte take-mask is widened
//    to 16 bits so it can drive the u16 owner blend.
//      - x86-64 (AVX2): vpblendvb is the blend, vpmovsxbw is the mask widen.
//      - aarch64 (NEON): bsl/bit are the blends, ushll/ushll2 are the mask widen
//        (.16b -> .8h). NEON is baseline on aarch64, so the target-cpu flag is
//        optional there.
//    Regressed to scalar if it prints nothing: the loop is then per-byte scalar
//    (x86: movzbl / setne / cmov or jumps; aarch64: ldrb / cmp / csel) with no
//    vector-lane registers (no xmm/ymm, no `.16b`/`.8h`) in the loop body.
//
// For more details, LLVM's vectorizer remarks say which loops it took or
// skipped and why (they are not attributed to a source line, so treat them as a
// hint, not proof):
//
//        RUSTFLAGS="-C target-cpu=native \
//          -C llvm-args=-pass-remarks=loop-vectorize \
//          -C llvm-args=-pass-remarks-missed=loop-vectorize" \
//          cargo build --release -p merge-lab 2>&1 | grep -i vector
//
//    "vectorized loop (vectorization width: 16 ...)" is the good case;
//    "vectorization is not beneficial" is the signature of the scalar regression.

use crate::{
    universe_priority_to_pap, Merger, OutBuf, Output, SourceIndex, SourceTable, MAX_SLOTS, NO_OWNER,
};

/// A merger that recomputes the entire output on every update.
#[derive(Debug)]
pub struct FullRemerge {
    /// Sources in ascending slot-index order (see [`SourceTable`]). Among
    /// sources equal on both priority and level, the lowest index wins.
    table: SourceTable,
    out: OutBuf,
}

impl FullRemerge {
    /// Recomputes every output slot from the current source states.
    ///
    /// Sources are visited in ascending index order with strictly-greater
    /// comparisons, so the lowest index wins a priority+level tie - identical to
    /// the `Naive` slot-serial reference in the oracle. The per-slot updates use
    /// arithmetic masks rather than `if`/`else`, which seems to nudge the
    /// compiler to autovectorize this loop (see the module docs).
    fn remerge(&mut self) {
        self.out.levels = [0; MAX_SLOTS];
        self.out.paps = [0; MAX_SLOTS];
        self.out.owners = [NO_OWNER; MAX_SLOTS];

        for src in self.table.sources() {
            let index = src.index;
            // A source contributes only to its valid leading slots; beyond that
            // its effective priority is `0` and it could never take a slot, so
            // the inner loop stops at `valid_level_count`. For these slots the
            // stored `pap` already equals `effective_pap`.
            for slot in 0..src.valid_level_count {
                let pap = src.pap[slot];
                let level = src.levels[slot];
                let best_pap = self.out.paps[slot];
                let best_level = self.out.levels[slot];

                let higher_pri = (pap > best_pap) as u8;
                let equal_pri = (pap == best_pap) as u8;
                let higher_level = (level > best_level) as u8;
                let nonzero = (pap != 0) as u8;
                let take = higher_pri | (equal_pri & higher_level & nonzero); // 0 or 1

                let m8 = take.wrapping_neg();
                let m16 = (take as u16).wrapping_neg();

                self.out.paps[slot] = (pap & m8) | (best_pap & !m8);
                self.out.levels[slot] = (level & m8) | (best_level & !m8);
                self.out.owners[slot] = (index & m16) | (self.out.owners[slot] & !m16);
            }
        }
    }
}

impl Merger for FullRemerge {
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
        // Zero everything beyond the supplied PAP so only `pap[..n]` is live;
        // this keeps the effective priority identical to the incremental impl.
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
