// Hand-written NEON full-re-merge (aarch64).
//
// Essentially the same algorithm as full_remerge.rs, but hand-written for
// NEON/aarch64.
//
// Output of slot owners causes a small wrinkle: levels and priorities are
// `u8`, so 16 fit in one `uint8x16_t`; owners are `u16`, so the same 16 slots
// need two `uint16x8_t` registers. The `u8` take-mask is therefore widened to
// two `u16` masks to drive the owner blends.
//
// NEON is mandatory on aarch64, so no runtime feature detection is needed; the
// intrinsics are simply `unsafe`.

use core::arch::aarch64::*;

use crate::{
    universe_priority_to_pap, Merger, OutBuf, Output, SourceIndex, SourceTable, MAX_SLOTS, NO_OWNER,
};

/// Slots processed per NEON vector (`uint8x16_t`).
const LANES: usize = 16;

/// A merger that recomputes the whole output each update using NEON.
#[derive(Debug)]
pub struct SimdRemerge {
    table: SourceTable,
    out: OutBuf,
}

impl SimdRemerge {
    /// Recomputes every output slot, 16 slots at a time, via NEON.
    fn remerge(&mut self) {
        // Lane index for masking the partial block at a source's
        // valid_level_count boundary.
        let lane_index: [u8; LANES] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

        // SAFETY: NEON is baseline on aarch64. All loads/stores stay within the
        // fixed-size 512-element buffers (block + LANES <= MAX_SLOTS).
        unsafe {
            let idxv = vld1q_u8(lane_index.as_ptr());

            for block in 0..(MAX_SLOTS / LANES) {
                let base = block * LANES;

                let mut best_level = vdupq_n_u8(0);
                let mut best_pap = vdupq_n_u8(0);
                let mut owner_lo = vdupq_n_u16(NO_OWNER);
                let mut owner_hi = vdupq_n_u16(NO_OWNER);

                for src in self.table.sources() {
                    let vlc = src.valid_level_count;
                    // Slots at or beyond valid_level_count contribute priority 0
                    // and can never win, so skip the source once past it.
                    if vlc <= base {
                        continue;
                    }

                    let mut pap = vld1q_u8(src.pap.as_ptr().add(base));
                    // Boundary block: zero the priority of lanes >= valid_level_count.
                    if vlc < base + LANES {
                        let remaining = (vlc - base) as u8;
                        let keep = vcltq_u8(idxv, vdupq_n_u8(remaining));
                        pap = vandq_u8(pap, keep);
                    }
                    let level = vld1q_u8(src.levels.as_ptr().add(base));

                    // take = (pap > best_pap) | (pap == best_pap & level > best_level & pap != 0)
                    let higher_pri = vcgtq_u8(pap, best_pap);
                    let equal_pri = vceqq_u8(pap, best_pap);
                    let higher_level = vcgtq_u8(level, best_level);
                    let nonzero = vtstq_u8(pap, pap);
                    let take = vorrq_u8(
                        higher_pri,
                        vandq_u8(vandq_u8(equal_pri, higher_level), nonzero),
                    );

                    best_level = vbslq_u8(take, level, best_level);
                    best_pap = vbslq_u8(take, pap, best_pap);

                    // Widen the u8 take-mask (0xFF/0x00) to two u16 masks
                    // (0xFFFF/0x0000) for the owner blends.
                    let take_lo = vcgtq_u16(vmovl_u8(vget_low_u8(take)), vdupq_n_u16(0));
                    let take_hi = vcgtq_u16(vmovl_u8(vget_high_u8(take)), vdupq_n_u16(0));
                    let id_vec = vdupq_n_u16(src.index);
                    owner_lo = vbslq_u16(take_lo, id_vec, owner_lo);
                    owner_hi = vbslq_u16(take_hi, id_vec, owner_hi);
                }

                vst1q_u8(self.out.levels.as_mut_ptr().add(base), best_level);
                vst1q_u8(self.out.paps.as_mut_ptr().add(base), best_pap);
                vst1q_u16(self.out.owners.as_mut_ptr().add(base), owner_lo);
                vst1q_u16(self.out.owners.as_mut_ptr().add(base + 8), owner_hi);
            }
        }
    }
}

// The mutators are identical to FullRemerge: they maintain the same per-source
// state and differ only in calling the NEON remerge.
impl Merger for SimdRemerge {
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
