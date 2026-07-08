// Adapter over the merger shipped in the `sacn` crate.
//
// `SacnMerger` implements the lab's `Merger` trait by delegating to
// `sacn::merger::DmxMerger`, so the production code becomes one more candidate
// the oracle can differential-test and the benchmarks can chart. The shipped
// merger is in the state-based tie-break family (it re-merges from scratch in
// ascending index order, like `FullRemerge`), so its owners match the `Naive`
// reference exactly.

use sacn::merger::DmxMerger;
use sacn::{Priority, SourceId};

use crate::{Merger, MergerHandle, Output, SourceIndex};

impl MergerHandle for SourceId {
    #[inline]
    fn index(self) -> SourceIndex {
        SourceId::index(self)
    }
}

/// The production `sacn` DMX merger, wrapped in the lab's [`Merger`] interface.
#[derive(Debug)]
pub struct SacnMerger {
    inner: DmxMerger,
}

impl Merger for SacnMerger {
    type Handle = SourceId;

    fn create() -> Self {
        Self {
            inner: DmxMerger::new(),
        }
    }

    fn add_source(&mut self) -> SourceId {
        self.inner.add_source().expect("using heap backing")
    }

    fn remove_source(&mut self, id: SourceId) {
        self.inner.remove_source(id);
    }

    fn update_levels(&mut self, id: SourceId, levels: &[u8]) {
        self.inner.update_levels(id, levels);
    }

    fn update_pap(&mut self, id: SourceId, pap: &[u8]) {
        self.inner.update_per_address_priorities(id, pap);
    }

    fn update_universe_priority(&mut self, id: SourceId, priority: u8) {
        // The lab only ever generates priorities in the valid 0..=200 range.
        let priority = Priority::new(priority).expect("universe priority out of range");
        self.inner.update_universe_priority(id, priority);
    }

    fn remove_pap(&mut self, id: SourceId) {
        self.inner.remove_per_address_priorities(id);
    }

    fn output(&self) -> Output<'_> {
        let out = self.inner.output();
        let owners = out.owners();
        // SAFETY: `SlotOwner` is `#[repr(transparent)]` over the raw owner index
        // (a `u16`), with `SlotOwner::NONE == u16::MAX`, which is exactly the
        // lab's `NO_OWNER`. A `&[SlotOwner]` therefore has identical layout and
        // values to the `&[SourceIndex]` the lab works in.
        let owners: &[SourceIndex] = unsafe {
            core::slice::from_raw_parts(owners.as_ptr().cast::<SourceIndex>(), owners.len())
        };
        Output {
            levels: out.levels(),
            paps: out.priorities(),
            owners,
        }
    }
}
