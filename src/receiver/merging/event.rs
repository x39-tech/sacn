//! Merged output and notification types produced by the [`Receiver`](super::Receiver).

#[cfg(feature = "alloc")]
use alloc::string::{String, ToString};
#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::net::SocketAddr;

use derive_where::derive_where;

use crate::merger::SlotOwner;
#[cfg(feature = "alloc")]
use crate::merger::SourceId;
#[cfg(feature = "alloc")]
use crate::receiver::event::{SourceInfo, UniverseData};
use crate::receiver::event::{SourceInfoRef, UniverseDataRef};
use crate::receiver::{BasicReceiverPollEvent, PollOutcome as BasicPollOutcome, ReceiverStorage};
use crate::storage::{MapLike, VecLike};
use crate::time::Instant;
use crate::types::{Cid, SourceName, Universe};

use super::Receiver;

// --- Owned events used by adapter layers ------------------------------------

/// A notification emitted by a [`Receiver`](super::Receiver).
///
/// This is the owned form, representing a combination of the events produced
/// by [`poll`](super::Receiver::poll), [`handle_packet`](super::Receiver::handle_packet)
/// and [`listen`](super::Receiver::listen), all gathered into one handy enum.
/// This is typically constructed and emitted by adapter layers.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiverEvent {
    /// A sampling period began for a universe. No merged data is emitted until
    /// the matching [`SamplingEnded`](ReceiverEvent::SamplingEnded).
    SamplingStarted {
        /// The universe whose sampling period began.
        universe: Universe,
    },

    /// A sampling period ended for a universe. The first live
    /// [`MergedData`](ReceiverEvent::MergedData) (if any sources are present)
    /// follows immediately.
    SamplingEnded {
        /// The universe whose sampling period ended.
        universe: Universe,
    },

    /// A new merged result for a universe.
    MergedData(MergedData),

    /// A new set of merged results for synchronized universes.
    SyncMergedData(Vec<MergedData>),

    /// Data carrying a START code other than NULL (levels) or per-address
    /// priority was received, forwarded untouched from a single source. Merging
    /// only applies to levels and priorities; everything else passes through.
    UniverseData(UniverseData),

    /// One or more sources stopped transmitting on a universe. Grouped the same
    /// way as the basic receiver groups near-simultaneous losses.
    SourcesLost {
        /// The universe the sources were lost on.
        universe: Universe,
        /// The sources that were lost.
        sources: Vec<MergedLostSource>,
    },

    /// A source stopped sending per-address priority while still sending levels;
    /// its priority has reverted to its universe priority in the merge.
    SourcePapLost {
        /// The universe on which per-address priority was lost.
        universe: Universe,
        /// The source that stopped sending per-address priority.
        source: SourceInfo,
    },

    /// The receiver ran out of room to track a new source on a universe.
    SourceLimitExceeded {
        /// The universe on which a source could not be tracked.
        universe: Universe,
    },

    /// The receiver ran out of room to track a new synchronization address.
    SyncLimitExceeded {
        /// The synchronization address that could not be tracked.
        sync_address: u16,
    },
}

// --- Merged sources ----------------------------------------------------------

/// One source contributing to a [`MergedData`] result, borrowing the receiver's
/// internal source table.
///
/// The owning counterpart is [`MergedSource`]; obtain one with
/// [`to_owned`](Self::to_owned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MergedSourceRef<'a> {
    /// The source's Component Identifier.
    pub cid: Cid,
    /// The source's human-readable name.
    pub name: &'a str,
    /// The address the source was last seen at.
    pub addr: SocketAddr,
    /// The source's most recent universe (packet) priority, as the raw 8-bit
    /// value from the wire.
    pub universe_priority: u8,
    /// Whether the source is currently sending per-address priority.
    pub per_address_priority_active: bool,
}

#[cfg(feature = "alloc")]
impl MergedSourceRef<'_> {
    /// Copies this into an owned [`MergedSource`].
    #[must_use]
    pub fn to_owned(&self) -> MergedSource {
        MergedSource {
            cid: self.cid,
            name: self.name.to_string(),
            addr: self.addr,
            universe_priority: self.universe_priority,
            per_address_priority_active: self.per_address_priority_active,
        }
    }
}

/// One source contributing to a [`MergedData`] result.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MergedSource {
    /// The source's Component Identifier.
    pub cid: Cid,
    /// The source's human-readable name.
    pub name: String,
    /// The address the source was last seen at.
    pub addr: SocketAddr,
    /// The source's most recent universe (packet) priority, as the raw 8-bit
    /// value from the wire.
    pub universe_priority: u8,
    /// Whether the source is currently sending per-address priority.
    pub per_address_priority_active: bool,
}

// --- Merged data -------------------------------------------------------------

/// A borrowed view of a universe's merged result.
///
/// The level, priority and owner slices borrow the receiver's merger buffers;
/// the source views borrow its source table. All are valid until the next
/// mutating call. The owning counterpart is [`MergedData`]; obtain one with
/// [`to_owned`](Self::to_owned).
///
/// Resolve the owner of a slot with [`source`](Self::source):
/// `data.source(data.owners()[slot])` yields the [`MergedSourceRef`] that won
/// that slot, or `None` if no source is sourcing it.
#[derive_where(Clone, Copy, Debug)]
pub struct MergedDataRef<'a, S: ReceiverStorage> {
    /// The universe the merged result is for.
    pub universe: Universe,
    levels: &'a [u8],
    priorities: &'a [u8],
    owners: &'a [SlotOwner],
    sources: &'a S::Sources,
}

impl<'a, S: ReceiverStorage> MergedDataRef<'a, S> {
    /// Builds a borrowed merge view from a universe's settled merger output and
    /// source table.
    pub(super) fn new(
        universe: Universe,
        levels: &'a [u8],
        priorities: &'a [u8],
        owners: &'a [SlotOwner],
        sources: &'a S::Sources,
    ) -> Self {
        Self {
            universe,
            levels,
            priorities,
            owners,
            sources,
        }
    }

    /// The merged level for each slot (512 entries).
    #[inline]
    pub fn levels(&self) -> &'a [u8] {
        self.levels
    }

    /// The winning priority for each slot (512 entries): the owning source's
    /// effective priority, or `0` where no source is sourcing the slot.
    #[inline]
    pub fn priorities(&self) -> &'a [u8] {
        self.priorities
    }

    /// The owning source for each slot (512 entries). Resolve an entry to its
    /// source with [`source`](Self::source).
    #[inline]
    pub fn owners(&self) -> &'a [SlotOwner] {
        self.owners
    }

    /// The sources currently contributing levels to the universe.
    pub fn active_sources(&self) -> impl Iterator<Item = MergedSourceRef<'a>> + '_ {
        self.sources
            .iter()
            .filter(|(_, src)| src.levels_active)
            .map(|(cid, src)| MergedSourceRef {
                cid: *cid,
                name: src.name.as_str(),
                addr: src.addr,
                universe_priority: src.universe_priority,
                per_address_priority_active: src.pap_active,
            })
    }

    /// Resolves the source that owns a slot (the value from [`owners`](Self::owners)).
    /// Returns `None` if no source is sourcing the slot.
    pub fn source(&self, owner: SlotOwner) -> Option<MergedSourceRef<'a>> {
        let id = owner.source()?;
        self.sources
            .iter()
            .find(|(_, src)| src.id == Some(id))
            .map(|(cid, src)| MergedSourceRef {
                cid: *cid,
                name: src.name.as_str(),
                addr: src.addr,
                universe_priority: src.universe_priority,
                per_address_priority_active: src.pap_active,
            })
    }

    /// Copies this into an owned [`MergedData`].
    #[cfg(feature = "alloc")]
    #[must_use]
    pub fn to_owned(&self) -> MergedData {
        let sources = self
            .sources
            .iter()
            .filter(|(_, src)| src.levels_active)
            .map(|(cid, src)| OwnedSource {
                id: src.id.expect("a levels-active source has a merger handle"),
                source: MergedSource {
                    cid: *cid,
                    name: src.name.to_string(),
                    addr: src.addr,
                    universe_priority: src.universe_priority,
                    per_address_priority_active: src.pap_active,
                },
            })
            .collect();
        MergedData {
            universe: self.universe,
            levels: self.levels.to_vec(),
            priorities: self.priorities.to_vec(),
            owners: self.owners.to_vec(),
            sources,
        }
    }
}

/// A tracked source paired with its merger handle, for owner resolution.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnedSource {
    id: SourceId,
    source: MergedSource,
}

/// An owned universe merge result.
///
/// The owning counterpart of [`MergedDataRef`].
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MergedData {
    /// The universe the merged result is for.
    pub universe: Universe,
    levels: Vec<u8>,
    priorities: Vec<u8>,
    owners: Vec<SlotOwner>,
    sources: Vec<OwnedSource>,
}

#[cfg(feature = "alloc")]
impl MergedData {
    /// The merged level for each slot (512 entries).
    #[inline]
    pub fn levels(&self) -> &[u8] {
        &self.levels
    }

    /// The winning priority for each slot (512 entries): the owning source's
    /// effective priority, or `0` where no source is sourcing the slot.
    #[inline]
    pub fn priorities(&self) -> &[u8] {
        &self.priorities
    }

    /// The owning source for each slot (512 entries). Resolve an entry to its
    /// source with [`source`](Self::source).
    #[inline]
    pub fn owners(&self) -> &[SlotOwner] {
        &self.owners
    }

    /// The sources currently contributing levels to the universe.
    pub fn active_sources(&self) -> impl Iterator<Item = &MergedSource> + '_ {
        self.sources.iter().map(|owned| &owned.source)
    }

    /// Resolves the source that owns a slot (the value from [`owners`](Self::owners)).
    /// Returns `None` if no source is sourcing the slot.
    pub fn source(&self, owner: SlotOwner) -> Option<&MergedSource> {
        let id = owner.source()?;
        self.sources
            .iter()
            .find(|owned| owned.id == id)
            .map(|owned| &owned.source)
    }
}

// --- Events and outcomes -----------------------------------------------------

/// A source reported lost by the merging [`Receiver`](super::Receiver).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MergedLostSource {
    /// The lost source's Component Identifier.
    pub cid: Cid,
    /// The lost source's human-readable name.
    pub name: SourceName,
    /// `true` if the source was lost because it sent a terminated stream, as
    /// opposed to timing out.
    pub terminated: bool,
}

/// The result of a single
/// [`Receiver::handle_packet`](super::Receiver::handle_packet) call.
#[non_exhaustive]
#[derive_where(Clone, Debug)]
pub enum MergedPacketOutcome<'r, 'p, S: ReceiverStorage> {
    /// The packet was not delivered to any listened universe: it was not a data
    /// or sync packet the receiver cares about, named an out-of-range or
    /// unlistened universe, or carried an unprocessed START code.
    Ignored,

    /// A data packet for `universe` whose new source could not be tracked
    /// because the per-universe source limit was reached. Nothing was merged.
    LimitExceeded {
        /// The universe the untrackable source targeted.
        universe: Universe,
    },

    /// A synchronization packet arrived on a new address that could not be
    /// tracked because the receiver's fixed-capacity synchronization-address
    /// table is full. Nothing was released.
    SyncLimitExceeded {
        /// The synchronization address that could not be tracked.
        sync_address: u16,
    },

    /// A levels or per-address-priority packet processed into `universe`'s merge.
    ///
    /// This variant might be produced with no meaningful data (i.e. `merged`
    /// is `None` and `pap_lost` is `None`); the semantic difference between
    /// that and `Ignored` or `LimitExceeded` is that `Data` means this packet
    /// was accepted and processed for a universe we care about, caused a state
    /// update, but no actual update is ready to be delivered. One example of
    /// when this can happen is when synchronization is active and a data
    /// packet is received and held for later sync.
    Data {
        /// The universe the packet was for.
        universe: Universe,
        /// The fresh merged result, if this packet published one. `None` while
        /// the universe is sampling, while its output is withheld under active
        /// synchronization, or when the packet changed nothing (e.g. a
        /// superseded sequence number).
        merged: Option<MergedDataRef<'r, S>>,
        /// A source that just lost per-address priority, reverting to its
        /// universe priority in the merge.
        pap_lost: Option<SourceInfoRef<'r>>,
    },

    /// An alternate-START-code packet forwarded untouched (it bypasses the
    /// merge).
    Passthrough {
        /// The forwarded per-source data.
        data: UniverseDataRef<'p>,
        /// Merged DMX data that this packet caused to be published. This can
        /// happen if the alternate-START-code packet "changed the universe's
        /// sync agreement, e.g. by setting the sync address to a new value or
        /// to 0. In this case, previously-held universe DMX data might now
        /// become active. This is a corner case and this is usually `None`.
        merged: Option<MergedDataRef<'r, S>>,
    },

    /// A universe synchronization packet. This results in merged data for
    /// each synchronized universe being delivered at once; read them via
    /// [`SyncRelease::merged_frames`].
    Sync(SyncRelease<'r, S>),
}

#[cfg(feature = "alloc")]
impl<S: ReceiverStorage> MergedPacketOutcome<'_, '_, S> {
    /// Calls `push` with the owned [`ReceiverEvent`] form of each event this
    /// outcome carries, in emission order.
    pub fn for_each_owned(&self, mut push: impl FnMut(ReceiverEvent)) {
        match self {
            MergedPacketOutcome::Ignored => {}
            MergedPacketOutcome::LimitExceeded { universe } => {
                push(ReceiverEvent::SourceLimitExceeded {
                    universe: *universe,
                });
            }
            MergedPacketOutcome::SyncLimitExceeded { sync_address } => {
                push(ReceiverEvent::SyncLimitExceeded {
                    sync_address: *sync_address,
                });
            }
            MergedPacketOutcome::Data {
                universe,
                merged,
                pap_lost,
            } => {
                if let Some(source) = pap_lost {
                    push(ReceiverEvent::SourcePapLost {
                        universe: *universe,
                        source: source.to_owned(),
                    });
                }
                if let Some(merged) = merged {
                    push(ReceiverEvent::MergedData(merged.to_owned()));
                }
            }
            MergedPacketOutcome::Passthrough { data, merged } => {
                push(ReceiverEvent::UniverseData(data.to_owned()));
                if let Some(merged) = merged {
                    push(ReceiverEvent::MergedData(merged.to_owned()));
                }
            }
            MergedPacketOutcome::Sync(release) => {
                let frames: Vec<MergedData> = release
                    .merged_frames()
                    .map(|frame| frame.to_owned())
                    .collect();
                // MergedPacketOutcome::Sync can produce 0 frames (like when
                // receiving the first sync packet on a new address) - suppress
                // this here
                if !frames.is_empty() {
                    push(ReceiverEvent::SyncMergedData(frames));
                }
            }
        }
    }
}

/// A synchronized release of merged DMX frames from each source using a given
/// synchronization address.
#[derive_where(Clone, Copy)]
pub struct SyncRelease<'r, S: ReceiverStorage> {
    receiver: &'r Receiver<S>,
}

impl<'r, S: ReceiverStorage> SyncRelease<'r, S> {
    pub(super) fn new(receiver: &'r Receiver<S>) -> Self {
        Self { receiver }
    }

    /// Lazy generator of the latched frames, one per released universe
    /// (possibly none - the first sync on an address releases nothing).
    pub fn merged_frames(&self) -> impl Iterator<Item = MergedDataRef<'r, S>> + 'r {
        let receiver = self.receiver;
        receiver
            .sync_release()
            .iter()
            .filter_map(move |&universe| receiver.merged(universe))
    }
}

impl<S: ReceiverStorage> core::fmt::Debug for SyncRelease<'_, S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SyncRelease")
            .field("released", &self.receiver.sync_release())
            .finish()
    }
}

/// A poll notification emitted by a [`Receiver`](super::Receiver).
///
/// A [`SourcesLost`](Self::SourcesLost) borrows the receiver's reusable loss
/// scratch, so it is valid only until the next
/// [`MergedPollOutcome::next_event`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiverPollEvent<'a> {
    /// A sampling period ended for a universe. This is mostly informational;
    /// if the universe has sources, a [`MergedDataChanged`](Self::MergedDataChanged)
    /// for it is also produced by the same poll.
    SamplingEnded {
        /// The universe whose sampling period ended.
        universe: Universe,
    },

    /// The merged result for a live (non-sampling) universe changed. Look up the
    /// new result with [`MergedPollOutcome::merged`] or
    /// [`Receiver::merged`](super::Receiver::merged).
    MergedDataChanged {
        /// The universe whose merged result changed.
        universe: Universe,
    },

    /// One or more sources stopped transmitting on a universe. Sources lost
    /// in quick succession are grouped into a single notification so the
    /// application can react to them together. Like [`SamplingEnded`](Self::SamplingEnded),
    /// this is mostly informational, as it will result in a
    /// [`MergedDataChanged`](Self::MergedDataChanged) representing the impact
    /// of the lost sources in the same poll.
    SourcesLost {
        /// The universe the sources were lost on.
        universe: Universe,
        /// The sources that were lost.
        sources: &'a [MergedLostSource],
    },
}

/// The outcome of a [`Receiver::poll`](super::Receiver::poll) call: the next
/// timer deadline plus a lazily-drained sequence of the events the poll produced.
///
/// The events are drawn out one at a time with [`next_event`](Self::next_event).
/// A returned [`SourcesLost`](ReceiverPollEvent::SourcesLost) borrows the
/// receiver's reusable loss scratch and is invalidated by the next call. A
/// [`MergedDataChanged`](ReceiverPollEvent::MergedDataChanged) event names a
/// universe whose merge changed; resolve it to the borrowed result with
/// [`merged`](Self::merged). Always drain all events from this struct, even if
/// you don't handle them!
pub struct MergedPollOutcome<'a, S: ReceiverStorage> {
    /// The earliest instant at which calling `poll` again could produce a
    /// different result (a timer deadline), or `None` if nothing is pending.
    pub deadline: Option<Instant>,
    basic: BasicPollOutcome<'a, S>,
    universes: &'a mut S::Universes,
    sync_addresses: &'a S::SyncAddresses,
    loss_scratch: &'a mut S::MergeLossList,
    now: Instant,
    phase: UniversePollPhase,
}

/// Which drain phase [`MergedPollOutcome::advance`] is in.
#[derive(Debug)]
enum UniversePollPhase {
    /// Pulling and translating the basic receiver's poll events.
    Basic,
    /// Draining the per-universe merged-change flags.
    MergedChanged,
    /// Every event delivered.
    Done,
}

/// Which event [`MergedPollOutcome::advance`] committed, for its tail to render.
enum Emit {
    SamplingEnded(Universe),
    SourcesLost(Universe),
    MergedChanged(Universe),
}

impl<S: ReceiverStorage> core::fmt::Debug for MergedPollOutcome<'_, S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MergedPollOutcome")
            .field("deadline", &self.deadline)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl<'a, S: ReceiverStorage> MergedPollOutcome<'a, S> {
    pub(super) fn new(
        deadline: Option<Instant>,
        basic: BasicPollOutcome<'a, S>,
        universes: &'a mut S::Universes,
        sync_addresses: &'a S::SyncAddresses,
        loss_scratch: &'a mut S::MergeLossList,
        now: Instant,
    ) -> Self {
        Self {
            deadline,
            basic,
            universes,
            sync_addresses,
            loss_scratch,
            now,
            phase: UniversePollPhase::Basic,
        }
    }

    /// The next event this poll produced, or `None` once every event has been
    /// drained.
    pub fn next_event(&mut self) -> Option<ReceiverPollEvent<'_>> {
        match self.advance()? {
            Emit::SamplingEnded(universe) => Some(ReceiverPollEvent::SamplingEnded { universe }),
            Emit::MergedChanged(universe) => {
                Some(ReceiverPollEvent::MergedDataChanged { universe })
            }
            Emit::SourcesLost(universe) => Some(ReceiverPollEvent::SourcesLost {
                universe,
                sources: self.loss_scratch.as_slice(),
            }),
        }
    }

    /// Pulls the next basic poll event and commits its merge-side effect, or (once
    /// the basic events are drained) emits one pending merged-change per universe.
    /// Reports which event to deliver by value, or `None` once fully drained.
    fn advance(&mut self) -> Option<Emit> {
        loop {
            match self.phase {
                UniversePollPhase::Basic => match self.basic.next_event() {
                    None => self.phase = UniversePollPhase::MergedChanged,
                    Some(BasicReceiverPollEvent::SamplingEnded { universe }) => {
                        super::deliver_sampling_ended::<S>(
                            self.universes,
                            self.sync_addresses,
                            universe,
                            self.now,
                        );
                        return Some(Emit::SamplingEnded(universe));
                    }
                    Some(BasicReceiverPollEvent::SourcesLost { universe, sources }) => {
                        super::deliver_sources_lost::<S>(
                            self.universes,
                            self.loss_scratch,
                            self.sync_addresses,
                            universe,
                            sources,
                            self.now,
                        );
                        return Some(Emit::SourcesLost(universe));
                    }
                },
                UniversePollPhase::MergedChanged => {
                    match super::take_pending_change::<S>(self.universes) {
                        Some(universe) => return Some(Emit::MergedChanged(universe)),
                        None => self.phase = UniversePollPhase::Done,
                    }
                }
                UniversePollPhase::Done => return None,
            }
        }
    }

    /// The current merged result for a universe, or `None` if it is not listened
    /// to or is still sampling. Use this to resolve a
    /// [`MergedDataChanged`](ReceiverPollEvent::MergedDataChanged) event without
    /// copying.
    #[must_use]
    pub fn merged(&self, universe: Universe) -> Option<MergedDataRef<'_, S>> {
        super::merged(&*self.universes, universe)
    }
}
