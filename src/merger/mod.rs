//! DMX merger: a software merger for DMX levels and priorities.
//!
//! This module provides the standalone [`DmxMerger`], which combines several
//! sources into a single per-slot output. It is the merge engine behind the
//! merging [`Receiver`](crate::receiver::Receiver) and is also usable on its
//! own. See [`DmxMerger`] for the merge semantics and a usage example.

#[cfg(test)]
mod tests;

use crate::error::Error;
use crate::storage::{coherence_check, HeapStorage, VecLike};
use crate::types::Priority;

// --- Storage types ----------------------------------------------------------

/// Storage types for a [`DmxMerger`].
pub trait MergerStorage: Sized {
    /// Backing for the dense, index-sorted live-source table.
    type MergeSources: VecLike<MergeSourceEntry>;
    /// Backing for the list of freed slot indices available for reuse.
    type FreeList: VecLike<SourceIndex>;
}

coherence_check! {
    /// Capacity coherence assertions for [`DmxMerger`].
    ///
    /// ```compile_fail
    /// use sacn::merger::{DmxMerger, MergeSourceEntry, MergerStorage, SourceIndex};
    /// struct Incoherent;
    /// impl MergerStorage for Incoherent {
    ///     type MergeSources = sacn::heapless::Vec<MergeSourceEntry, 8>;
    ///     type FreeList = sacn::heapless::Vec<SourceIndex, 4>;
    /// }
    /// let _ = DmxMerger::<Incoherent>::default();
    /// ```
    AssertCoherent<S: MergerStorage> = {
        assert!(
            <S::FreeList as VecLike<SourceIndex>>::CAPACITY
                >= <S::MergeSources as VecLike<MergeSourceEntry>>::CAPACITY,
            "MergerStorage::FreeList capacity must be >= MergeSources capacity",
        );
    }
}

#[cfg(feature = "alloc")]
impl MergerStorage for HeapStorage {
    type MergeSources = alloc::vec::Vec<MergeSourceEntry>;
    type FreeList = alloc::vec::Vec<SourceIndex>;
}

#[cfg(not(feature = "alloc"))]
impl MergerStorage for HeapStorage {
    type MergeSources = heapless::Vec<MergeSourceEntry, 0>;
    type FreeList = heapless::Vec<SourceIndex, 0>;
}

/// The number of DMX slots in one universe.
const MAX_SLOTS: usize = 512;

/// A source's compact slot index: the value written to the per-slot owner output
/// identifying which source won a slot.
///
/// We use the smallest type we can get away with (`u16`) to help with
/// vectorization.
#[doc(hidden)]
pub type SourceIndex = u16;

/// The raw owner value reserved for a slot that no source is sourcing. The
/// public face of this sentinel is [`SlotOwner::NONE`].
const NO_OWNER: SourceIndex = SourceIndex::MAX;

/// The source that won an output slot, or none.
///
/// One is returned per slot by [`MergeOutput::owners`]. It is a niche over a
/// [`SourceId`]: it occupies the same two bytes as a source's index but
/// reserves one value to mean "no source is sourcing this slot".
///
/// Pull out the winning [`SourceId`] with [`source`](Self::source) and compare
/// it directly against the source you care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SlotOwner(SourceIndex);

impl SlotOwner {
    /// The owner of a slot that no source is sourcing.
    pub const NONE: SlotOwner = SlotOwner(NO_OWNER);

    /// The winning [`SourceId`], or `None` if no source is sourcing the slot.
    #[inline]
    pub fn source(self) -> Option<SourceId> {
        (self != Self::NONE).then_some(SourceId(self.0))
    }

    /// Whether some source is sourcing this slot.
    #[inline]
    pub fn is_some(self) -> bool {
        self != Self::NONE
    }

    /// Whether no source is sourcing this slot.
    #[inline]
    pub fn is_none(self) -> bool {
        self == Self::NONE
    }
}

/// Converts a universe priority to the per-slot priority used in the merge.
///
/// A universe priority of `0` is treated as a per-address priority of `1` so
/// that a source advertising the lowest universe priority still competes (a
/// per-address priority of `0` means "not sourcing").
#[inline]
fn universe_priority_to_pap(universe_priority: u8) -> u8 {
    if universe_priority == 0 {
        1
    } else {
        universe_priority
    }
}

/// A handle to a source in a [`DmxMerger`], returned by
/// [`add_source`](DmxMerger::add_source) and accepted by every mutator.
///
/// A source's compact slot index is reused after the source is removed, so a
/// `SourceId` is valid only while its source is live, much like an index into a
/// `Vec`. The mutators ignore a handle whose source has been removed, but once
/// that slot has been reused by a later [`add_source`](DmxMerger::add_source)
/// the old handle addresses the *new* source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceId(SourceIndex);

impl SourceId {
    /// The source's compact slot index: the value this source writes to the
    /// owner output while it is live.
    ///
    /// Use it to relate an entry in [`MergeOutput::owners`] back to the source
    /// that owns that slot. The index is unique among the sources currently in
    /// the merger, but is reused once a source is removed, so only compare it
    /// against the output of the same merger while this handle is live.
    #[inline]
    pub fn index(self) -> SourceIndex {
        self.0
    }
}

/// One source's tracked state.
///
/// `pap` holds the *effective* per-slot priority; for each slot, the PAP for
/// that slot or the universe priority if PAP is not being sent. A source
/// contributes to a slot only when both `slot < valid_level_count` and
/// `pap[slot] > 0`.
#[doc(hidden)]
#[derive(Debug)]
pub struct MergeSourceEntry {
    /// This source's compact slot index: its owner-output value, and its sort
    /// key within [`SourceTable`].
    index: SourceIndex,
    /// Per-slot levels.
    levels: [u8; MAX_SLOTS],
    /// Per-slot effective priority (PAP, or universe priority converted to PAP).
    pap: [u8; MAX_SLOTS],
    /// Number of leading slots for which this source has supplied levels. Slots
    /// at or beyond this index never contribute, regardless of `pap`.
    valid_level_count: usize,
    /// Number of leading slots for which explicit PAP has been supplied. Used
    /// only to zero the correct region when a shorter PAP packet arrives.
    pap_count: usize,
    /// Whether the source's priority currently comes from its universe priority
    /// (`true`) or from explicit PAP (`false`).
    using_universe_priority: bool,
    /// The most recent universe priority (pre-conversion).
    universe_priority: u8,
}

impl MergeSourceEntry {
    /// Creates a source that is sourcing nothing yet.
    fn new(index: SourceIndex) -> Self {
        Self {
            index,
            levels: [0; MAX_SLOTS],
            pap: [0; MAX_SLOTS],
            valid_level_count: 0,
            pap_count: 0,
            using_universe_priority: true,
            universe_priority: 0,
        }
    }
}

/// Source storage and handle allocation.
///
/// Sources live in a dense `Vec` kept sorted ascending by [`Source::index`], so
/// the merge hot loop walks contiguous memory and the tie-break (lowest index
/// wins a priority+level tie) is deterministic. A removed source's index is
/// freed and handed to the next [`add`](Self::add), keeping the index space
/// bounded by the concurrent source count.
#[derive(Debug)]
struct SourceTable<S: MergerStorage> {
    /// Live sources, sorted ascending by `MergeSourceEntry::index`.
    sources: S::MergeSources,
    /// Freed slot indices available for reuse (LIFO).
    free: S::FreeList,
}

impl<S: MergerStorage> Default for SourceTable<S> {
    fn default() -> Self {
        Self {
            sources: S::MergeSources::default(),
            free: S::FreeList::default(),
        }
    }
}

impl<S: MergerStorage> SourceTable<S> {
    fn new() -> Self {
        Self::default()
    }

    /// The live sources as a slice, sorted ascending by index.
    #[inline]
    fn sources(&self) -> &[MergeSourceEntry] {
        self.sources.as_slice()
    }

    /// A mutable reference to the live source at dense position `pos`.
    #[inline]
    fn source_mut(&mut self, pos: usize) -> &mut MergeSourceEntry {
        &mut self.sources.as_mut_slice()[pos]
    }

    /// Allocates a source and returns its handle.
    ///
    /// Reuses a freed slot when one is available, otherwise grows the index
    /// space by one. Returns [`Error::NoCapacity`] when the source backing is
    /// full, or when [`NO_OWNER`] worth of sources are live at once (65535),
    /// the ceiling the owner sentinel reserves.
    fn add(&mut self) -> Result<SourceId, Error> {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                // The free list is empty, so every index ever allocated is
                // currently live: the next fresh index is the live count.
                let index = self.sources.len();
                if index >= NO_OWNER as usize {
                    return Err(Error::NoCapacity);
                }
                index as SourceIndex
            }
        };
        // Insert keeping `sources` sorted by index; the slot is free, so no equal
        // key exists and `binary_search` always reports the insertion point.
        let pos = self
            .sources()
            .binary_search_by_key(&index, |s| s.index)
            .unwrap_err();
        self.sources
            .insert(pos, MergeSourceEntry::new(index))
            .map_err(|_| Error::NoCapacity)?;
        Ok(SourceId(index))
    }

    /// Resolves a [`SourceId`] to its dense position in `sources`, or `None`
    /// when no live source holds the handle's index.
    #[inline]
    fn resolve(&self, handle: SourceId) -> Option<usize> {
        self.sources()
            .binary_search_by_key(&handle.0, |s| s.index)
            .ok()
    }

    /// Removes the source at dense position `pos`, freeing its slot for reuse.
    fn remove_at(&mut self, pos: usize) {
        let index = self.sources().get(pos).expect("valid position").index;
        self.sources.remove(pos);
        self.free
            .push(index)
            .expect("free list holds at most the merger capacity");
    }
}

/// Fixed-size output storage for the merge result.
#[derive(Debug)]
struct OutBuf {
    /// Winning level per slot.
    levels: [u8; MAX_SLOTS],
    /// Winning priority per slot.
    paps: [u8; MAX_SLOTS],
    /// Owning source per slot ([`SlotOwner::NONE`] when unsourced).
    owners: [SlotOwner; MAX_SLOTS],
}

impl OutBuf {
    /// Creates an all-unsourced output buffer.
    fn empty() -> Self {
        Self {
            levels: [0; MAX_SLOTS],
            paps: [0; MAX_SLOTS],
            owners: [SlotOwner::NONE; MAX_SLOTS],
        }
    }
}

/// A borrowed view of a [`DmxMerger`]'s output: one entry per slot, 512 slots.
///
/// Obtain one from [`DmxMerger::output`]. The slices borrow the merger's
/// internal buffers and are valid until the next mutating call.
#[derive(Debug, Clone, Copy)]
pub struct MergeOutput<'a> {
    levels: &'a [u8],
    priorities: &'a [u8],
    owners: &'a [SlotOwner],
}

impl<'a> MergeOutput<'a> {
    /// The winning level for each slot (512 entries).
    #[inline]
    pub fn levels(&self) -> &'a [u8] {
        self.levels
    }

    /// The winning priority for each slot (512 entries).
    ///
    /// This is the per-address priority the merger would transmit: it equals the
    /// winning source's effective priority, or `0` where no source is sourcing
    /// the slot. A universe priority of `0` that wins a slot is reported as `1`.
    #[inline]
    pub fn priorities(&self) -> &'a [u8] {
        self.priorities
    }

    /// The owning source for each slot (512 entries): the source that won the
    /// slot, or [`SlotOwner::NONE`] where no source is sourcing it.
    #[inline]
    pub fn owners(&self) -> &'a [SlotOwner] {
        self.owners
    }
}

/// A standalone DMX merger.
///
/// A `DmxMerger` combines several *sources*, each offering up to 512 DMX levels
/// plus a per-slot priority, into a single output. For each slot the winner is
/// the source with the highest priority; ties are broken by the highest level
/// (Highest-Takes-Precedence, HTP). The output records, per slot, the winning
/// level, the winning priority, and the *owner* (which source won). A source
/// competes for a slot only when it has a non-zero priority there.
///
/// Priority comes from one of two places. A source may send explicit per-address
/// priorities (PAP, one byte per slot), or a single universe priority that
/// applies to every slot. PAP, once present, overrides the universe priority
/// until removed with
/// [`remove_per_address_priorities`](Self::remove_per_address_priorities). There
/// is a deliberate quirk in how the lowest priority is interpreted: the lowest
/// universe priority is `0`, but the lowest per-address priority is `1`, because
/// a per-address priority of `0` means "not sourcing this slot". A universe
/// priority of `0` is therefore treated as a per-address priority of `1` so that
/// a source advertising the lowest universe priority still competes (BSR
/// E1.31-1 / ETC's per-address-priority extension).
///
/// # Driving the merger
///
/// The merger merges synchronously each time it is mutated. Add each source with
/// [`add_source`](Self::add_source), feed it data with the `update_*` methods,
/// and read the merge result back from [`output`](Self::output):
///
/// ```
/// use sacn::merger::DmxMerger;
/// use sacn::Priority;
///
/// let mut merger = DmxMerger::new();
/// let a = merger.add_source().unwrap();
/// let b = merger.add_source().unwrap();
///
/// merger.update_universe_priority(a, Priority::new(100).unwrap());
/// merger.update_universe_priority(b, Priority::new(100).unwrap());
/// merger.update_levels(a, &[10, 200]);
/// merger.update_levels(b, &[100, 100]);
///
/// let out = merger.output();
/// // Slot 0: b wins (higher level at equal priority). Slot 1: a wins.
/// assert_eq!(&out.levels()[..2], &[100, 200]);
/// assert_eq!(out.owners()[0].source(), Some(b));
/// assert_eq!(out.owners()[1].source(), Some(a));
/// ```
///
/// Every mutating call recomputes the full merge result, so [`output`](Self::output)
/// always reflects the current state of every source. The recompute is a
/// branch-free pass over all sources and slots that the compiler can
/// autovectorize on platforms which support it. Realistic worst-case
/// sources-per-universe and data-update scenarios perform comfortably within
/// a single universe's frame budget.
#[derive(Debug)]
pub struct DmxMerger<S: MergerStorage = HeapStorage> {
    /// Sources in ascending slot-index order. Among sources equal on both
    /// priority and level, the lowest index wins.
    table: SourceTable<S>,
    out: OutBuf,
    deferred: bool,
}

impl<S: MergerStorage> Default for DmxMerger<S> {
    /// Creates an empty merger with all output slots unsourced.
    fn default() -> Self {
        let () = AssertCoherent::<S>::CHECK;
        Self {
            table: SourceTable::new(),
            out: OutBuf::empty(),
            deferred: false,
        }
    }
}

#[cfg(feature = "alloc")]
impl DmxMerger<HeapStorage> {
    /// Creates an empty, heap-backed merger with all output slots unsourced.
    ///
    /// For a fixed-capacity merger, construct with
    /// `DmxMerger::<Caps>::default()` constructing the capacity policy `Caps`
    /// using [`static_storage!`](crate::static_storage!).
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S: MergerStorage> DmxMerger<S> {
    /// Recomputes the output unless recomputes are currently deferred.
    #[inline]
    fn maybe_remerge(&mut self) {
        if !self.deferred {
            self.remerge();
        }
    }

    /// Sets whether output recomputes are deferred.
    ///
    /// While deferred, the `update_*` and `remove_*` mutators still record each
    /// source's new state but do not recompute the merge, so [`output`](Self::output)
    /// keeps returning the frame computed before deferral began - the input is
    /// applied without affecting output. Clearing deferral recomputes once,
    /// publishing a single coherent frame that reflects every update made while
    /// deferred.
    ///
    /// This is generally used for sACN universe synchronization.
    pub fn set_deferred(&mut self, deferred: bool) {
        if self.deferred && !deferred {
            self.deferred = false;
            self.remerge();
        } else {
            self.deferred = deferred;
        }
    }

    /// Whether output recomputes are currently deferred (see
    /// [`set_deferred`](Self::set_deferred)).
    pub fn is_deferred(&self) -> bool {
        self.deferred
    }

    /// Recomputes the output immediately from the current source states,
    /// regardless of the [`deferred`](Self::set_deferred) setting.
    pub fn recompute(&mut self) {
        self.remerge();
    }

    /// Adds a new source and returns its handle.
    ///
    /// Returns [`Error::NoCapacity`] when the merger's source backing is full.
    pub fn add_source(&mut self) -> Result<SourceId, Error> {
        self.table.add()
    }

    /// Removes a source, releasing any slots it owned and recomputing the output.
    ///
    /// A stale or unknown handle is ignored.
    pub fn remove_source(&mut self, id: SourceId) {
        if let Some(i) = self.table.resolve(id) {
            self.table.remove_at(i);
            self.maybe_remerge();
        }
    }

    /// Sets the source's levels (slots `0..levels.len()`), recomputing the
    /// output.
    ///
    /// Slots beyond `levels.len()` are treated as having a level of `0` and never
    /// contribute. At most 512 levels (one universe) are read. A stale or
    /// unknown handle is ignored.
    pub fn update_levels(&mut self, id: SourceId, levels: &[u8]) {
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
        self.maybe_remerge();
    }

    /// Sets the source's explicit per-address priorities (PAP), recomputing the
    /// output.
    ///
    /// This makes PAP active for the source: its universe priority no longer
    /// affects the merge until [`remove_per_address_priorities`](Self::remove_per_address_priorities)
    /// is called. Slots beyond `pap.len()` are treated as a PAP of `0` ("not
    /// sourcing"). At most 512 values (one universe) are read. A stale or
    /// unknown handle is ignored.
    pub fn update_per_address_priorities(&mut self, id: SourceId, pap: &[u8]) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let n = pap.len().min(MAX_SLOTS);
        let s = self.table.source_mut(i);
        s.using_universe_priority = false;
        s.pap[..n].copy_from_slice(&pap[..n]);
        // Zero everything beyond the supplied PAP so only `pap[..n]` is live.
        s.pap[n..].fill(0);
        s.pap_count = n;
        self.maybe_remerge();
    }

    /// Sets the source's universe priority, recomputing the output.
    ///
    /// Has no effect on the merge while explicit PAP is active for the source
    /// (see [`update_per_address_priorities`](Self::update_per_address_priorities)),
    /// though the new value is retained and applies once PAP is removed. A stale
    /// or unknown handle is ignored.
    pub fn update_universe_priority(&mut self, id: SourceId, priority: Priority) {
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
        self.maybe_remerge();
    }

    /// Removes explicit PAP from a source, reverting it to its universe priority,
    /// and recomputes the output.
    ///
    /// A stale or unknown handle is ignored.
    pub fn remove_per_address_priorities(&mut self, id: SourceId) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let s = self.table.source_mut(i);
        s.using_universe_priority = true;
        s.pap.fill(universe_priority_to_pap(s.universe_priority));
        s.pap_count = MAX_SLOTS;
        self.maybe_remerge();
    }

    /// Returns the current merge result. See [`MergeOutput`].
    pub fn output(&self) -> MergeOutput<'_> {
        MergeOutput {
            levels: &self.out.levels,
            priorities: &self.out.paps,
            owners: &self.out.owners,
        }
    }

    /// Recomputes every output slot from the current source states.
    ///
    /// Sources are visited in ascending index order with strictly-greater
    /// comparisons, so the lowest index wins a priority+level tie. The per-slot
    /// updates use arithmetic masks rather than `if`/`else`, which lets the
    /// compiler autovectorize this loop.
    fn remerge(&mut self) {
        self.out.levels = [0; MAX_SLOTS];
        self.out.paps = [0; MAX_SLOTS];
        self.out.owners = [SlotOwner::NONE; MAX_SLOTS];

        for src in self.table.sources() {
            let index = src.index;
            // A source contributes only to its valid leading slots; beyond that
            // its effective priority is `0` and it could never take a slot, so
            // the inner loop stops at `valid_level_count`. For these slots the
            // stored `pap` already equals the effective priority.
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
                self.out.owners[slot] = SlotOwner((index & m16) | (self.out.owners[slot].0 & !m16));
            }
        }
    }
}
