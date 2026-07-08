// Hand-written AVX2 / SSE2 full-re-merge (x86_64).
//
// Essentially the same algorithm as full_remerge.rs, but hand-written for
// AVX2/SSE2 on x86_64.
//
// Unlike NEON, which is baseline on aarch64 and so needs no feature detection,
// AVX2 is not guaranteed on x86_64. To measure the *fastest* the machine can
// go, this detects AVX2 once at construction and dispatches to a
// 32-slots-at-a-time AVX2 kernel when present, falling back to a
// 16-slots-at-a-time SSE2 kernel (SSE2 is baseline in the x86_64 ABI) otherwise.
// Both kernels produce identical results; only the vector width differs.
//
// Wrinkles relative to the NEON implementation:
//
// - No unsigned byte compare. The x86 `cmpgt` ops are signed, but levels and
//   priorities are `u8` spanning the full `0..=255` range. The `cmpgt_epu8_*`
//   helpers bias both operands by `0x80` (an XOR that maps the unsigned order
//   onto the signed order) before the signed compare. Equality (`cmpeq_epi8`) is
//   bitwise and needs no bias.
// - Blending. AVX2 has `blendv_epi8` (NEON's `vbslq` equivalent); SSE2 has no
//   variable blend, so the SSE2 kernel open-codes `(a & mask) | (b & ~mask)`.
// - Widening the take-mask for owners. Owners are `u16`, so the byte take-mask
//   is widened to two `u16` masks to drive the owner blends. SSE2 uses
//   `unpacklo/hi_epi8(take, take)`; the AVX2 kernel sign-extends each 128-bit
//   half with `cvtepi8_epi16` because AVX2's `unpack` is lane-local and would not
//   keep the 32 slots in order.

use core::arch::x86_64::*;

use crate::{
    universe_priority_to_pap, Merger, OutBuf, Output, SourceIndex, SourceTable, MAX_SLOTS, NO_OWNER,
};

/// A merger that recomputes the whole output each update using AVX2 (or SSE2).
#[derive(Debug)]
pub struct SimdRemerge {
    table: SourceTable,
    out: OutBuf,
    /// Whether the running CPU supports AVX2, decided once at construction.
    has_avx2: bool,
}

// --- AVX2 helpers (32-wide) --------------------------------------------------

/// Per-lane unsigned `a > b` for `u8` lanes (AVX2).
///
/// AVX2 only has signed byte comparison, so bias both operands by `0x80`: the XOR
/// flips the sign bit, mapping the unsigned `0..=255` order onto the signed order,
/// after which `cmpgt_epi8` yields the unsigned result.
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn cmpgt_epu8_avx2(a: __m256i, b: __m256i) -> __m256i {
    let bias = _mm256_set1_epi8(0x80u8 as i8);
    _mm256_cmpgt_epi8(_mm256_xor_si256(a, bias), _mm256_xor_si256(b, bias))
}

// --- SSE2 helpers (16-wide) --------------------------------------------------

/// Per-lane unsigned `a > b` for `u8` lanes (SSE2). See [`cmpgt_epu8_avx2`].
#[inline]
unsafe fn cmpgt_epu8_sse2(a: __m128i, b: __m128i) -> __m128i {
    let bias = _mm_set1_epi8(0x80u8 as i8);
    _mm_cmpgt_epi8(_mm_xor_si128(a, bias), _mm_xor_si128(b, bias))
}

/// Per-lane select `mask ? a : b` for SSE2, which has no variable blend.
///
/// `mask` is an all-ones / all-zeros lane mask; the result is `(a & mask) | (b &
/// ~mask)`.
#[inline]
unsafe fn select_sse2(mask: __m128i, a: __m128i, b: __m128i) -> __m128i {
    _mm_or_si128(_mm_and_si128(mask, a), _mm_andnot_si128(mask, b))
}

impl SimdRemerge {
    /// Recomputes every output slot via the best vector kernel the CPU supports.
    fn remerge(&mut self) {
        // SAFETY: `has_avx2` comes from `is_x86_feature_detected!("avx2")` at
        // construction, so the AVX2 kernel runs only when the CPU supports it;
        // the SSE2 kernel uses only x86_64-baseline instructions.
        unsafe {
            if self.has_avx2 {
                self.remerge_avx2();
            } else {
                self.remerge_sse2();
            }
        }
    }

    /// AVX2 kernel: 32 slots per vector (`__m256i`).
    #[target_feature(enable = "avx2")]
    unsafe fn remerge_avx2(&mut self) {
        /// Slots processed per AVX2 vector.
        const LANES: usize = 32;

        // Lane index {0,1,...,31} for masking the partial block at a source's
        // valid_level_count boundary.
        #[rustfmt::skip]
        let lane_index: [u8; LANES] = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
        ];
        let idxv = _mm256_loadu_si256(lane_index.as_ptr() as *const __m256i);
        let zero = _mm256_setzero_si256();

        for block in 0..(MAX_SLOTS / LANES) {
            let base = block * LANES;

            let mut best_level = zero;
            let mut best_pap = zero;
            // Owners are u16, so the 32 slots need two vectors (16 lanes each).
            let mut owner_lo = _mm256_set1_epi16(NO_OWNER as i16);
            let mut owner_hi = _mm256_set1_epi16(NO_OWNER as i16);

            for src in self.table.sources() {
                let vlc = src.valid_level_count;
                // Slots at or beyond valid_level_count contribute priority 0 and
                // can never win, so skip the source once past it.
                if vlc <= base {
                    continue;
                }

                let mut pap = _mm256_loadu_si256(src.pap.as_ptr().add(base) as *const __m256i);
                // Boundary block: zero the priority of lanes >= valid_level_count.
                if vlc < base + LANES {
                    let remaining = (vlc - base) as u8;
                    // keep = lane_index < remaining, i.e. remaining > lane_index.
                    let keep = cmpgt_epu8_avx2(_mm256_set1_epi8(remaining as i8), idxv);
                    pap = _mm256_and_si256(pap, keep);
                }
                let level = _mm256_loadu_si256(src.levels.as_ptr().add(base) as *const __m256i);

                // take = (pap > best_pap) | (pap == best_pap & level > best_level & pap != 0)
                let higher_pri = cmpgt_epu8_avx2(pap, best_pap);
                let equal_pri = _mm256_cmpeq_epi8(pap, best_pap);
                let higher_level = cmpgt_epu8_avx2(level, best_level);
                let is_zero = _mm256_cmpeq_epi8(pap, zero);
                let tie_win =
                    _mm256_andnot_si256(is_zero, _mm256_and_si256(equal_pri, higher_level));
                let take = _mm256_or_si256(higher_pri, tie_win);

                best_level = _mm256_blendv_epi8(best_level, level, take);
                best_pap = _mm256_blendv_epi8(best_pap, pap, take);

                // Widen the u8 take-mask to two u16 masks for the owner blends.
                // AVX2's unpack is lane-local, so sign-extend each 128-bit half
                // separately: bytes 0..15 drive owner_lo, bytes 16..31 owner_hi.
                let take_lo = _mm256_cvtepi8_epi16(_mm256_castsi256_si128(take));
                let take_hi = _mm256_cvtepi8_epi16(_mm256_extracti128_si256(take, 1));
                let id_vec = _mm256_set1_epi16(src.index as i16);
                owner_lo = _mm256_blendv_epi8(owner_lo, id_vec, take_lo);
                owner_hi = _mm256_blendv_epi8(owner_hi, id_vec, take_hi);
            }

            _mm256_storeu_si256(
                self.out.levels.as_mut_ptr().add(base) as *mut __m256i,
                best_level,
            );
            _mm256_storeu_si256(
                self.out.paps.as_mut_ptr().add(base) as *mut __m256i,
                best_pap,
            );
            _mm256_storeu_si256(
                self.out.owners.as_mut_ptr().add(base) as *mut __m256i,
                owner_lo,
            );
            _mm256_storeu_si256(
                self.out.owners.as_mut_ptr().add(base + 16) as *mut __m256i,
                owner_hi,
            );
        }
    }

    /// SSE2 fallback kernel: 16 slots per vector (`__m128i`).
    unsafe fn remerge_sse2(&mut self) {
        /// Slots processed per SSE2 vector.
        const LANES: usize = 16;

        let lane_index: [u8; LANES] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let idxv = _mm_loadu_si128(lane_index.as_ptr() as *const __m128i);
        let zero = _mm_setzero_si128();

        for block in 0..(MAX_SLOTS / LANES) {
            let base = block * LANES;

            let mut best_level = zero;
            let mut best_pap = zero;
            // Owners are u16, so the 16 slots need two vectors (8 lanes each).
            let mut owner_lo = _mm_set1_epi16(NO_OWNER as i16);
            let mut owner_hi = _mm_set1_epi16(NO_OWNER as i16);

            for src in self.table.sources() {
                let vlc = src.valid_level_count;
                if vlc <= base {
                    continue;
                }

                let mut pap = _mm_loadu_si128(src.pap.as_ptr().add(base) as *const __m128i);
                if vlc < base + LANES {
                    let remaining = (vlc - base) as u8;
                    let keep = cmpgt_epu8_sse2(_mm_set1_epi8(remaining as i8), idxv);
                    pap = _mm_and_si128(pap, keep);
                }
                let level = _mm_loadu_si128(src.levels.as_ptr().add(base) as *const __m128i);

                let higher_pri = cmpgt_epu8_sse2(pap, best_pap);
                let equal_pri = _mm_cmpeq_epi8(pap, best_pap);
                let higher_level = cmpgt_epu8_sse2(level, best_level);
                let is_zero = _mm_cmpeq_epi8(pap, zero);
                let tie_win = _mm_andnot_si128(is_zero, _mm_and_si128(equal_pri, higher_level));
                let take = _mm_or_si128(higher_pri, tie_win);

                best_level = select_sse2(take, level, best_level);
                best_pap = select_sse2(take, pap, best_pap);

                // Widen the u8 take-mask to two u16 masks: unpacking the mask with
                // itself duplicates each 0xFF/0x00 byte into a 0xFFFF/0x0000 lane.
                let take_lo = _mm_unpacklo_epi8(take, take);
                let take_hi = _mm_unpackhi_epi8(take, take);
                let id_vec = _mm_set1_epi16(src.index as i16);
                owner_lo = select_sse2(take_lo, id_vec, owner_lo);
                owner_hi = select_sse2(take_hi, id_vec, owner_hi);
            }

            _mm_storeu_si128(
                self.out.levels.as_mut_ptr().add(base) as *mut __m128i,
                best_level,
            );
            _mm_storeu_si128(
                self.out.paps.as_mut_ptr().add(base) as *mut __m128i,
                best_pap,
            );
            _mm_storeu_si128(
                self.out.owners.as_mut_ptr().add(base) as *mut __m128i,
                owner_lo,
            );
            _mm_storeu_si128(
                self.out.owners.as_mut_ptr().add(base + 8) as *mut __m128i,
                owner_hi,
            );
        }
    }
}

// The mutators are identical to FullRemerge: they maintain the same per-source
// state and differ only in calling the SIMD remerge.
impl Merger for SimdRemerge {
    type Handle = SourceIndex;

    fn create() -> Self {
        Self {
            table: SourceTable::new(),
            out: OutBuf::empty(),
            has_avx2: std::is_x86_feature_detected!("avx2"),
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
