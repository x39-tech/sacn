//! Source-loss settling: grouping sources lost in quick succession.
//!
//! When a source is determined to be lost, the receiver does not report it
//! immediately. Instead it opens a **termination set** capturing that source
//! plus every other source whose online/offline status is not yet known, and
//! holds the notification until either the whole set is confirmed offline or a
//! captured source proves to still be online. This algorithm avoids a brief,
//! wrong "winner" when several sources drop at once.
//!
//! A set fires only once every captured source is confirmed offline (the
//! settling window, 0-2.5s, as each captured source either resolves offline or
//! proves still online). An optional caller-configured extra hold time can be
//! applied on top as the set's [`wait_expiry`](TerminationSet::wait_expiry),
//! delaying the notification further; it defaults to zero and never affects the
//! grouping itself.

use crate::storage::{MapLike, VecLike};
use crate::time::{Duration, Instant};
use crate::types::Cid;

use super::{BasicReceiverStorage, LostSource};

/// A group of sources whose loss is being settled together.
#[doc(hidden)]
pub struct TerminationSet<S: BasicReceiverStorage> {
    /// The earliest time the set may produce a notification. The set still does
    /// not fire until every member is confirmed offline.
    wait_expiry: Instant,
    /// The sources captured in this set, keyed by CID.
    sources: S::TermSetSources,
}

/// One captured source within a [`TerminationSet`].
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct TerminationSetSource {
    /// `true` once the source has been confirmed lost; `false` while its status
    /// is still unknown.
    offline: bool,
    /// Whether the loss was due to stream termination (only meaningful once
    /// `offline`).
    terminated: bool,
}

/// A source confirmed lost this tick: its CID and whether it terminated.
pub(super) type OfflineSource = (Cid, bool);
/// A source whose status is unknown this tick (alive but silent): its CID.
pub(super) type UnknownSource = Cid;

impl<S: BasicReceiverStorage> TerminationSet<S> {
    /// The earliest instant this set may fire a notification.
    pub(super) fn wait_expiry(&self) -> Instant {
        self.wait_expiry
    }
}

impl<S: BasicReceiverStorage> core::fmt::Debug for TerminationSet<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TerminationSet")
            .field("wait_expiry", &self.wait_expiry)
            .field("num_sources", &self.sources.len())
            .finish()
    }
}

/// Whether any existing termination set already contains `cid`.
fn already_tracked<S: BasicReceiverStorage>(sets: &S::TermSets, cid: &Cid) -> bool {
    sets.iter().any(|ts| ts.sources.contains_key(cid))
}

/// Finds the (mutable) record of `cid` across all termination sets, if any.
fn find_mut<'a, S: BasicReceiverStorage + 'a>(
    sets: &'a mut S::TermSets,
    cid: &Cid,
) -> Option<&'a mut TerminationSetSource> {
    sets.iter_mut().find_map(|ts| ts.sources.get_mut(cid))
}

/// Processes the sources confirmed lost this tick, creating or extending
/// termination sets as needed.
///
/// A source already in a set is just marked offline. A newly lost source opens
/// a fresh set that also captures every `unknown` source not already held by
/// some other set, so they can settle together.
pub(super) fn mark_sources_offline<S: BasicReceiverStorage>(
    offline: &[OfflineSource],
    unknown: &[UnknownSource],
    sets: &mut S::TermSets,
    extra_hold_time: Duration,
    now: Instant,
) {
    for (cid, terminated) in offline {
        if let Some(existing) = find_mut::<S>(sets, cid) {
            if !existing.offline {
                existing.offline = true;
                existing.terminated = *terminated;
            }
            continue;
        }

        let mut new_set: TerminationSet<S> = TerminationSet {
            wait_expiry: now.saturating_add(extra_hold_time),
            sources: S::TermSetSources::default(),
        };
        new_set.sources.upsert_expect(
            *cid,
            TerminationSetSource {
                offline: true,
                terminated: *terminated,
            },
        );

        for ucid in unknown {
            if !already_tracked::<S>(sets, ucid) && !new_set.sources.contains_key(ucid) {
                new_set.sources.upsert_expect(
                    *ucid,
                    TerminationSetSource {
                        offline: false,
                        terminated: false,
                    },
                );
            }
        }

        // A full fixed-capacity term-set list drops the new set; the source will
        // be re-observed offline on the next tick and captured then.
        sets.push_expect(new_set);
    }
}

/// Removes sources confirmed online this tick from every termination set,
/// dropping any set that becomes empty.
pub(super) fn mark_sources_online<S: BasicReceiverStorage>(online: &[Cid], sets: &mut S::TermSets) {
    for cid in online {
        for ts in sets.iter_mut() {
            ts.sources.remove(cid);
        }
    }
    sets.retain(|ts| !ts.sources.is_empty());
}

/// Fires any termination set whose wait has elapsed and whose members are all
/// confirmed offline, writing the lost sources into `out` (cleared first) and
/// removing the fired sets.
///
/// A set with a still-unknown member is left in place: the notification waits
/// until that member resolves (online, and is removed, or offline, and joins
/// the rest).
pub(super) fn get_expired_sources<S: BasicReceiverStorage>(
    sets: &mut S::TermSets,
    now: Instant,
    out: &mut S::LossList,
) {
    out.clear();
    sets.retain(|ts| {
        let ready = now >= ts.wait_expiry && ts.sources.values().all(|s| s.offline);
        if ready {
            for (cid, src) in ts.sources.iter() {
                out.push_expect(LostSource {
                    cid: *cid,
                    terminated: src.terminated,
                });
            }
            false
        } else {
            true
        }
    });
}
