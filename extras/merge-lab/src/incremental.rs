// Each mutation causes an incremental merge.
//
// "Incremental" means work is avoided wherever possible by checking
// invariants. For example, if a source which was already winning increases
// its priority, the other sources do not need to be considered.
//
// This is a port of ETC's `dmx_merger.c` algorithm, with one deliberate
// change: sources live in a flat, ascending-id `Vec` instead of a red-black
// tree of separately-allocated nodes. The algorithm itself mimics the source
// material.
//
// Function names and comments generally track the ETC originals for ease of
// reference.

use crate::{
    universe_priority_to_pap, Merger, OutBuf, Output, Source, SourceIndex, SourceTable, MAX_SLOTS,
    NO_OWNER,
};

/// A merger that updates outputs using incremental logic.
#[derive(Debug)]
pub struct Incremental {
    /// Sources in ascending slot-index order (see [`SourceTable`]).
    table: SourceTable,
    out: OutBuf,
}

impl Incremental {
    // --- levels -------------------------------------------------------------

    fn update_levels_single(&mut self, i: usize, new_levels: &[u8], old: usize, new: usize) {
        {
            let s = &mut self.table.sources_mut()[i];
            s.levels[..new].copy_from_slice(new_levels);
            if old > new {
                s.levels[new..old].fill(0);
            }
        }
        let s = &self.table.sources()[i];
        let out = &mut self.out;

        // Update levels for slots this source is sourcing.
        for slot in 0..new {
            if s.pap[slot] > 0 {
                out.levels[slot] = new_levels[slot];
            }
        }
        // Newly valid slots also need priority/owner published.
        if new > old {
            for slot in old..new {
                if s.pap[slot] > 0 {
                    out.paps[slot] = s.pap[slot];
                    out.owners[slot] = s.index;
                }
            }
        }
        // Slots that fell out of range are released.
        if old > new {
            out.levels[new..old].fill(0);
            out.paps[new..old].fill(0);
            out.owners[new..old].fill(NO_OWNER);
        }
    }

    fn update_levels_multi(&mut self, i: usize, new_levels: &[u8], old: usize, new: usize) {
        {
            let s = &mut self.table.sources_mut()[i];
            s.levels[..new].copy_from_slice(new_levels);
            if old > new {
                s.levels[new..old].fill(0);
            }
        }

        // HTP merge over slots valid both before and after the update.
        let min = old.min(new);
        {
            let sources = self.table.sources();
            let out = &mut self.out;
            let s = &sources[i];
            for slot in 0..min {
                let sp = s.pap[slot];
                // Only relevant when this source shares the current winning priority.
                if sp > 0 && sp == out.paps[slot] {
                    if s.levels[slot] > out.levels[slot] {
                        // Higher level: take ownership.
                        out.levels[slot] = s.levels[slot];
                        out.owners[slot] = s.index;
                    } else if s.index == out.owners[slot] && s.levels[slot] < out.levels[slot] {
                        // We owned this slot and our level dropped: rescan for a
                        // new owner among the equally-prioritized sources.
                        out.levels[slot] = s.levels[slot];
                        for (c, cand) in sources.iter().enumerate() {
                            if c == i {
                                continue;
                            }
                            if cand.pap[slot] == out.paps[slot]
                                && cand.levels[slot] > out.levels[slot]
                            {
                                out.levels[slot] = cand.levels[slot];
                                out.owners[slot] = cand.index;
                            }
                        }
                    }
                }
            }
        }

        // Publish priorities for slots that just became valid (grow), and
        // release slots that just became invalid (shrink). One range is empty.
        merge_new_priorities(self.table.sources(), i, &mut self.out, old, new);
        merge_new_priorities(self.table.sources(), i, &mut self.out, new, old);
    }

    // --- per-address priority ----------------------------------------------

    fn update_pap_single(&mut self, i: usize, new_pap: &[u8], old: usize, new: usize) {
        {
            let s = &mut self.table.sources_mut()[i];
            s.pap[..new].copy_from_slice(new_pap);
            if old > new {
                s.pap[new..old].fill(0);
            }
        }
        let s = &self.table.sources()[i];
        let out = &mut self.out;
        let vlc = s.valid_level_count;
        out.paps[..vlc].copy_from_slice(&s.pap[..vlc]);
        for slot in 0..vlc {
            if s.pap[slot] == 0 {
                out.levels[slot] = 0;
                out.owners[slot] = NO_OWNER;
            } else {
                out.levels[slot] = s.levels[slot];
                out.owners[slot] = s.index;
            }
        }
    }

    fn update_pap_multi(&mut self, i: usize, new_pap: &[u8], old: usize, new: usize) {
        {
            let s = &mut self.table.sources_mut()[i];
            s.pap[..new].copy_from_slice(new_pap);
            if old > new {
                s.pap[new..old].fill(0);
            }
        }
        let vlc = self.table.sources()[i].valid_level_count;
        merge_new_priorities(self.table.sources(), i, &mut self.out, 0, vlc);
    }

    // --- universe priority --------------------------------------------------

    fn update_up_single(&mut self, i: usize, pap: u8) {
        self.table.sources_mut()[i].pap.fill(pap);
        let s = &self.table.sources()[i];
        let out = &mut self.out;
        let vlc = s.valid_level_count;
        out.paps[..vlc].fill(pap);
        for slot in 0..vlc {
            out.owners[slot] = s.index;
        }
        out.levels[..vlc].copy_from_slice(&s.levels[..vlc]);
    }

    fn update_up_multi(&mut self, i: usize, pap: u8) {
        self.table.sources_mut()[i].pap.fill(pap);
        let vlc = self.table.sources()[i].valid_level_count;
        merge_new_priorities(self.table.sources(), i, &mut self.out, 0, vlc);
    }
}

/// Merge a source's priority over `[start, end)`, recomputing affected outputs.
///
/// Three cases per slot: the source outranks the current winner (take it); the
/// source ties the winner on priority and beats it on level (take it); or the
/// source *is* the current owner and its priority dropped (rescan every other
/// source for the new winner).
fn merge_new_priorities(sources: &[Source], i: usize, out: &mut OutBuf, start: usize, end: usize) {
    let src = &sources[i];
    for slot in start..end {
        let src_pap = src.effective_pap(slot);

        if src_pap > out.paps[slot] {
            // Strictly higher priority: take ownership.
            out.levels[slot] = src.levels[slot];
            out.owners[slot] = src.index;
            out.paps[slot] = src_pap;
        } else if src.index != out.owners[slot] {
            // Not the owner: take only on equal (non-zero) priority, higher level.
            if src_pap > 0 && src_pap == out.paps[slot] && src.levels[slot] > out.levels[slot] {
                out.levels[slot] = src.levels[slot];
                out.owners[slot] = src.index;
            }
        } else if src_pap < out.paps[slot] {
            // We owned this slot and our priority dropped: rescan for a new owner.
            out.paps[slot] = src_pap;
            if out.paps[slot] == 0 {
                out.levels[slot] = 0;
                out.owners[slot] = NO_OWNER;
            }
            for (c, cand) in sources.iter().enumerate() {
                if c == i {
                    continue;
                }
                let cand_pap = cand.effective_pap(slot);
                if cand_pap > out.paps[slot]
                    || (cand_pap > 0
                        && cand_pap == out.paps[slot]
                        && cand.levels[slot] > out.levels[slot])
                {
                    out.levels[slot] = cand.levels[slot];
                    out.owners[slot] = cand.index;
                    out.paps[slot] = cand_pap;
                }
            }
        }
    }
}

impl Merger for Incremental {
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
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        // Merge the source out with all-zero priority, then drop it.
        self.table.sources_mut()[i].pap.fill(0);
        merge_new_priorities(self.table.sources(), i, &mut self.out, 0, MAX_SLOTS);
        self.table.remove_at(i);
    }

    fn update_levels(&mut self, id: SourceIndex, new_levels: &[u8]) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let new = new_levels.len();
        let old = self.table.sources()[i].valid_level_count;

        self.table.sources_mut()[i].valid_level_count = new;
        if new == old && &self.table.sources()[i].levels[..new] == new_levels {
            return;
        }

        if self.table.len() == 1 {
            self.update_levels_single(i, new_levels, old, new);
        } else {
            self.update_levels_multi(i, new_levels, old, new);
        }
    }

    fn update_pap(&mut self, id: SourceIndex, new_pap: &[u8]) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let new = new_pap.len();
        let old = {
            let s = &mut self.table.sources_mut()[i];
            s.using_universe_priority = false;
            let old = s.pap_count;
            s.pap_count = new;
            old
        };
        if new == old && &self.table.sources()[i].pap[..new] == new_pap {
            return;
        }
        if self.table.len() == 1 {
            self.update_pap_single(i, new_pap, old, new);
        } else {
            self.update_pap_multi(i, new_pap, old, new);
        }
    }

    fn update_universe_priority(&mut self, id: SourceIndex, priority: u8) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        {
            let s = &mut self.table.sources_mut()[i];
            if priority == s.universe_priority && !s.universe_priority_uninitialized {
                return;
            }
            s.universe_priority_uninitialized = false;
            s.universe_priority = priority;
        }
        if self.table.sources()[i].using_universe_priority {
            self.table.sources_mut()[i].pap_count = MAX_SLOTS;
            let pap = universe_priority_to_pap(priority);
            if self.table.len() == 1 {
                self.update_up_single(i, pap);
            } else {
                self.update_up_multi(i, pap);
            }
        }
    }

    fn remove_pap(&mut self, id: SourceIndex) {
        let Some(i) = self.table.resolve(id) else {
            return;
        };
        let pap = {
            let s = &mut self.table.sources_mut()[i];
            s.using_universe_priority = true;
            s.pap_count = MAX_SLOTS;
            universe_priority_to_pap(s.universe_priority)
        };
        self.table.sources_mut()[i].pap.fill(pap);
        let vlc = self.table.sources()[i].valid_level_count;
        merge_new_priorities(self.table.sources(), i, &mut self.out, 0, vlc);
    }

    fn output(&self) -> Output<'_> {
        self.out.as_output()
    }
}
