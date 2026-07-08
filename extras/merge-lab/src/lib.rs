// A lab for measuring DMX-merger implementation strategies against each other.
//
// This crate was built to answer a design question for the merger (sACN's
// per-address-priority+HTP merge): which implementation strategy is fastest,
// and by how much. `FullRemerge` was chosen as the winner despite not being
// fastest in all cases (see the crate README for a discussion of the
// tradeoffs) and it is what ships in the `sacn` crate. This lab is preserved
// as the reproducible record of that decision, with the other candidates kept
// frozen for comparison. See the crate README for the results and how to
// re-run the face-off.
//
// A merger combines several *sources*, each offering up to [`MAX_SLOTS`] DMX
// levels plus a per-slot priority, into one output. For each slot the winner is
// the source with the highest priority; ties are broken by the highest level
// (Highest-Takes-Precedence, HTP). The output records, per slot, the winning
// level, the winning priority, and the owner (which source won). A source only
// competes for a slot if it has a non-zero priority there.
//
// Priority comes from one of two places. A source may send explicit
// per-address priorities (PAP, one byte per slot), or a single universe
// priority that is converted to a per-slot priority for every slot (with the
// protocol quirk that universe priority `0` converts to a PAP of `1`). PAP,
// once present, overrides the universe priority until removed. A priority of
// `0` at a slot means "not sourcing this slot".
//
// All candidates implementations implement the Merger trait so a single
// benchmark harness and a single correctness oracle can drive them identically:
//
// - `FullRemerge` recomputes every slot from scratch on each update, a
//   straightforward scalar double-loop which can be autovectorized by the
//   compiler (see its comment).
// - `IncrementalFlat` is a faithful port of ETC's incremental algorithm (it
//   touches only the slots that can have changed), over a flat `Vec` of
//   sources rather than ETC's red-black tree.
// - `SacnMerger` is a thin adapter over the merger actually shipped in the
//   `sacn` crate (at time of writing, identical to `FullRemerge`)

// The merge code uses deliberate index-based loops: in many places indexing
// reads more clearly than iterator adapters.
#![allow(clippy::needless_range_loop)]

#[cfg(feature = "etc")]
mod etc_ffi;
mod full_remerge;
mod incremental;
mod sacn_merger;
#[cfg(all(feature = "simd", target_arch = "aarch64"))]
mod simd_remerge_aarch64;
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
mod simd_remerge_x86;
pub mod workload;

#[cfg(feature = "etc")]
pub use etc_ffi::EtcMerger;
pub use full_remerge::FullRemerge;
pub use incremental::Incremental;
pub use sacn_merger::SacnMerger;
#[cfg(all(feature = "simd", target_arch = "aarch64"))]
pub use simd_remerge_aarch64::SimdRemerge;
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
pub use simd_remerge_x86::SimdRemerge;

/// The number of DMX slots in one universe.
pub const MAX_SLOTS: usize = 512;

/// A source's compact slot index: the value written to the per-slot owner
/// output, identifying which source won a slot.
///
/// This indexes *concurrent* sources, not lifetime allocations: a removed
/// source's slot is reused (see [`SourceTable`]). Its width is `u16`
/// because the owner output's element width drives the SIMD owner blend.
pub type SourceIndex = u16;

/// The owner value written for a slot that no source is sourcing.
pub const NO_OWNER: SourceIndex = SourceIndex::MAX;

/// A source handle, as produced and consumed by a [`Merger`].
///
/// For compatibility with the production `sacn` implementation.
pub trait MergerHandle: Copy {
    /// The compact owner index this handle's source writes to the output.
    fn index(self) -> SourceIndex;
}

impl MergerHandle for SourceIndex {
    #[inline]
    fn index(self) -> SourceIndex {
        self
    }
}

/// Converts a universe priority to the per-slot priority used in the merge.
///
/// A universe priority of `0` is treated as a per-address priority of `1` so
/// that a source advertising the lowest universe priority still competes.
#[inline]
pub fn universe_priority_to_pap(universe_priority: u8) -> u8 {
    if universe_priority == 0 {
        1
    } else {
        universe_priority
    }
}

/// A borrowed view of a merger's output: one entry per slot.
///
/// `levels` and `paps` are the winning level and winning priority per slot;
/// `owners` is the winning [`SourceIndex`], or [`NO_OWNER`] where no source is
/// sourcing the slot.
#[derive(Debug, Clone, Copy)]
pub struct Output<'a> {
    /// The winning level for each slot.
    pub levels: &'a [u8],
    /// The winning priority for each slot.
    pub paps: &'a [u8],
    /// The owning source for each slot, or [`NO_OWNER`].
    pub owners: &'a [SourceIndex],
}

/// The merge interface shared by every candidate implementation.
///
/// Each mutating call leaves [`output`](Merger::output) reflecting the full
/// merge result.
pub trait Merger {
    /// The handle type this merger hands out and accepts.
    type Handle: MergerHandle;

    /// Creates an empty merger with all output slots unsourced.
    fn create() -> Self
    where
        Self: Sized;

    /// Adds a new source and returns its handle.
    fn add_source(&mut self) -> Self::Handle;

    /// Removes a source, releasing any slots it owned.
    fn remove_source(&mut self, id: Self::Handle);

    /// Sets the source's levels (slot `0..levels.len()`), recomputing outputs.
    fn update_levels(&mut self, id: Self::Handle, levels: &[u8]);

    /// Sets the source's explicit per-address priorities, recomputing outputs.
    ///
    /// This makes PAP active for the source: its universe priority no longer
    /// affects the merge until [`remove_pap`](Merger::remove_pap) is called.
    fn update_pap(&mut self, id: Self::Handle, pap: &[u8]);

    /// Sets the source's universe priority, recomputing outputs.
    ///
    /// Has no effect on the merge while PAP is active for the source.
    fn update_universe_priority(&mut self, id: Self::Handle, priority: u8);

    /// Removes explicit PAP, reverting the source to its universe priority.
    fn remove_pap(&mut self, id: Self::Handle);

    /// Returns the current merge result.
    fn output(&self) -> Output<'_>;
}

/// Fixed-size output storage shared by the candidate implementations.
#[derive(Debug)]
pub struct OutBuf {
    /// Winning level per slot.
    pub levels: [u8; MAX_SLOTS],
    /// Winning priority per slot.
    pub paps: [u8; MAX_SLOTS],
    /// Owning source per slot ([`NO_OWNER`] when unsourced).
    pub owners: [SourceIndex; MAX_SLOTS],
}

impl OutBuf {
    /// Creates an all-unsourced output buffer.
    pub fn empty() -> Self {
        Self {
            levels: [0; MAX_SLOTS],
            paps: [0; MAX_SLOTS],
            owners: [NO_OWNER; MAX_SLOTS],
        }
    }

    /// Borrows the buffer as an [`Output`].
    pub fn as_output(&self) -> Output<'_> {
        Output {
            levels: &self.levels,
            paps: &self.paps,
            owners: &self.owners,
        }
    }
}

/// One source's tracked state, shared by all implementations.
///
/// `pap` holds the *effective* per-slot priority already accounting for the
/// universe-priority conversion, so the merge logic reads a single array. A
/// source contributes to a slot only when both `slot < valid_level_count` and
/// `pap[slot] > 0`; see [`Source::effective_pap`].
#[derive(Debug)]
pub struct Source {
    /// This source's slot index: its owner-output value, and (for the flat
    /// implementations) its sort key within [`SourceTable`].
    pub index: SourceIndex,
    /// Per-slot levels.
    pub levels: [u8; MAX_SLOTS],
    /// Per-slot effective priority (PAP, or universe priority converted to PAP).
    pub pap: [u8; MAX_SLOTS],
    /// Number of leading slots for which this source has supplied levels. Slots
    /// at or beyond this index never contribute, regardless of `pap`.
    pub valid_level_count: usize,
    /// Number of leading slots for which explicit PAP has been supplied. Used
    /// only to zero the correct region when a shorter PAP packet arrives.
    pub pap_count: usize,
    /// Whether the source's priority currently comes from its universe priority
    /// (true) or from explicit PAP (false).
    pub using_universe_priority: bool,
    /// The most recent universe priority (pre-conversion).
    pub universe_priority: u8,
    /// Whether a universe priority has yet to be applied. Lets the first
    /// `update_universe_priority(0)` take effect even though `0` is the default.
    pub universe_priority_uninitialized: bool,
}

impl Source {
    /// Creates a source that is sourcing nothing yet.
    pub fn new(index: SourceIndex) -> Self {
        Self {
            index,
            levels: [0; MAX_SLOTS],
            pap: [0; MAX_SLOTS],
            valid_level_count: 0,
            pap_count: 0,
            using_universe_priority: true,
            universe_priority: 0,
            universe_priority_uninitialized: true,
        }
    }

    /// The priority this source contributes at `slot`: its stored `pap`, but `0`
    /// for slots beyond `valid_level_count`.
    #[inline]
    pub fn effective_pap(&self, slot: usize) -> u8 {
        if slot < self.valid_level_count {
            self.pap[slot]
        } else {
            0
        }
    }
}

/// Source storage and handle allocation shared by all implementations.
///
/// Sources live in a dense `Vec` kept sorted ascending by [`Source::index`], so
/// the merge hot loop walks contiguous memory and the tie-break (lowest index
/// wins a priority+level tie) is deterministic. A removed source's index is
/// freed and handed to the next [`add`](Self::add), keeping the index space
/// bounded by the concurrent source count.
#[derive(Debug, Default)]
pub struct SourceTable {
    /// Live sources, sorted ascending by `Source::index`.
    sources: Vec<Source>,
    /// Freed slot indices available for reuse (LIFO).
    free: Vec<SourceIndex>,
}

impl SourceTable {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    #[inline]
    pub fn sources_mut(&mut self) -> &mut [Source] {
        &mut self.sources
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Allocates a source and returns its index.
    ///
    /// Reuses a freed slot when one is available, otherwise grows the index
    /// space by one. Panics only when [`NO_OWNER`] worth of sources are live at
    /// once (65535), which the owner sentinel reserves and which no realistic
    /// sACN topology approaches.
    pub fn add(&mut self) -> SourceIndex {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                // The free list is empty, so every index ever allocated is
                // currently live: the next fresh index is the live count.
                let index = self.sources.len();
                assert!(
                    index < NO_OWNER as usize,
                    "merge-lab: exceeded {} concurrent sources",
                    NO_OWNER
                );
                index as SourceIndex
            }
        };
        // Insert keeping `sources` sorted by index; the slot is free, so no equal
        // key exists and `binary_search` always reports the insertion point.
        let pos = self
            .sources
            .binary_search_by_key(&index, |s| s.index)
            .unwrap_err();
        self.sources.insert(pos, Source::new(index));
        index
    }

    /// Resolves a source index to its dense position in [`sources`](Self::sources),
    /// or `None` when no live source holds that index.
    #[inline]
    pub fn resolve(&self, index: SourceIndex) -> Option<usize> {
        self.sources.binary_search_by_key(&index, |s| s.index).ok()
    }

    /// Removes the source at dense index `pos`, freeing its slot for reuse.
    pub fn remove_at(&mut self, pos: usize) {
        let index = self.sources[pos].index;
        self.sources.remove(pos);
        self.free.push(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_index_is_reused_after_removal() {
        let mut t = SourceTable::new();
        let a = t.add();
        let b = t.add();
        assert_eq!((a, b), (0, 1));

        // Removing `a` frees slot 0; the next add reuses it rather than growing
        // the index space - this is what keeps the owner index bounded by
        // concurrency instead of lifetime allocations.
        t.remove_at(t.resolve(a).unwrap());
        assert_eq!(
            t.resolve(a),
            None,
            "a removed source's handle must not resolve while its slot is free"
        );

        let c = t.add();
        assert_eq!(c, 0);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn sources_stay_sorted_by_index_through_reuse() {
        let mut t = SourceTable::new();
        let _a = t.add(); // 0
        let b = t.add(); // 1
        let _c = t.add(); // 2
        t.remove_at(t.resolve(b).unwrap()); // frees slot 1
        let _d = t.add(); // reuses slot 1, must insert between 0 and 2

        let indices: Vec<_> = t.sources().iter().map(|s| s.index).collect();
        assert_eq!(indices, vec![0, 1, 2]);
    }
}
