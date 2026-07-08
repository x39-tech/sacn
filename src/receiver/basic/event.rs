//! Notifications and outcomes produced by the [`BasicReceiver`](super::BasicReceiver).

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "alloc")]
use crate::receiver::event::{SourceInfo, UniverseData};
use crate::receiver::event::{SourceInfoRef, UniverseDataRef};
use crate::time::Instant;
use crate::types::{Cid, Universe};

use super::{BasicReceiver, BasicReceiverStorage};

// --- Owned events used by adapter layers ------------------------------------

/// A notification emitted by a [`BasicReceiver`](super::BasicReceiver).
///
/// This is the owned form, representing a combination of the events produced
/// by [`poll`](super::BasicReceiver::poll), [`handle_packet`](super::BasicReceiver::handle_packet),
/// and [`listen`](super::BasicReceiver::listen), all gathered into one handy
/// enum. This is typically constructed and emitted by adapter layers.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BasicReceiverEvent {
    /// A sampling period began for a universe. Data reported during the period
    /// carries [`UniverseData::is_sampling`]` == true`; an application that
    /// wants a flicker-free first frame should wait for the matching
    /// [`SamplingEnded`](BasicReceiverEvent::SamplingEnded) before acting on it.
    SamplingStarted {
        /// The universe whose sampling period began.
        universe: Universe,
    },

    /// A sampling period ended for a universe; level data received from all
    /// sources on this universe should now be reconciled and acted upon.
    SamplingEnded {
        /// The universe whose sampling period ended.
        universe: Universe,
    },

    /// A data packet was accepted from a source. Note that there is no "new
    /// source" event - this event will be sent on the first data packet from
    /// a new source. Carries the per-source levels or alternate start code
    /// data for the universe.
    UniverseData(UniverseData),

    /// One or more sources stopped transmitting on a universe (by timing out or
    /// by sending a terminated stream). Sources lost in quick succession are
    /// grouped into a single notification so the application can react to them
    /// together.
    SourcesLost {
        /// The universe the sources were lost on.
        universe: Universe,
        /// The sources that were lost.
        sources: Vec<LostSource>,
    },

    /// A source that had been sending per-address priority (`0xDD`) data stopped
    /// sending it while still sending levels. Priority resolution for that
    /// source should fall back to its packet (universe) priority.
    SourcePapLost {
        /// The universe on which per-address priority was lost.
        universe: Universe,
        /// The source that stopped sending per-address priority.
        source: SourceInfo,
    },

    /// The receiver ran out of room to track a new source on a universe. Emitted
    /// at most once until the number of tracked sources drops below the limit
    /// and is exceeded again.
    SourceLimitExceeded {
        /// The universe on which a source could not be tracked.
        universe: Universe,
    },

    /// A universe synchronization packet was received. An application driving a
    /// [`BasicReceiver`](super::BasicReceiver) that wants conformant
    /// synchronization holds the data it has buffered for every universe whose
    /// [`sync_address`](crate::receiver::UniverseData::sync_address) equals this
    /// [`sync_address`](Self::SyncReceived::sync_address) and releases it now.
    /// Only emitted when synchronization is enabled
    /// ([`ReceiverConfig::with_synchronization`](crate::receiver::ReceiverConfig::with_synchronization)).
    SyncReceived {
        /// The synchronization universe the packet was sent on: the address
        /// whose held data it releases.
        sync_address: u16,
        /// The source that sent the synchronization packet.
        cid: Cid,
    },
}

#[cfg(feature = "alloc")]
impl From<BasicReceiverPollEvent<'_>> for BasicReceiverEvent {
    fn from(value: BasicReceiverPollEvent<'_>) -> Self {
        match value {
            BasicReceiverPollEvent::SamplingEnded { universe } => {
                BasicReceiverEvent::SamplingEnded { universe }
            }
            BasicReceiverPollEvent::SourcesLost { universe, sources } => {
                BasicReceiverEvent::SourcesLost {
                    universe,
                    sources: sources.to_vec(),
                }
            }
        }
    }
}

/// A source reported in a [`SourcesLost`](BasicReceiverEvent::SourcesLost)
/// notification.
///
/// A lost source is identified by its [`cid`](Self::cid); the basic receiver
/// does not retain the human-readable name (it is carried per-packet in
/// [`UniverseData`] while the source is live). The merging
/// [`Receiver`](crate::receiver::Receiver), which keeps per-source state for the
/// merge, does report the name - see
/// [`MergedLostSource`](crate::receiver::MergedLostSource).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LostSource {
    /// The lost source's Component Identifier.
    pub cid: Cid,
    /// `true` if the source was lost because it sent a terminated stream, as
    /// opposed to timing out.
    pub terminated: bool,
}

// --- Borrowed packet outcome -------------------------------------------------

/// The events produced by a single
/// [`handle_packet`](super::BasicReceiver::handle_packet) call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PacketOutcome<'a> {
    /// The packet was ignored: it was not a data or sync packet the receiver
    /// cares about, named an out-of-range or unlistened universe, or carried an
    /// unprocessed START code.
    Ignored,

    /// A data packet for `universe` whose new source could not be tracked
    /// because the per-universe source limit was reached. Nothing was delivered.
    LimitExceeded {
        /// The universe the untrackable source targeted.
        universe: Universe,
    },

    /// A data packet accepted for `universe`. This variant might carry no
    /// deliverable data (both `data` and `pap_lost` are `None`) when the packet
    /// was accepted but withheld, terminated, or superseded; the difference from
    /// `Ignored` is that the packet pertained to a universe we care about.
    Data {
        /// The universe the packet pertained to.
        universe: Universe,
        /// Per-source data to deliver, if the packet produced any. Note that the
        /// first data received from a new source serves as the notification that
        /// that source is now being tracked.
        data: Option<UniverseDataRef<'a>>,
        /// The source that just lost per-address priority, if any.
        pap_lost: Option<SourceInfoRef<'a>>,
    },

    /// A synchronization packet was received.
    Sync {
        /// The synchronization universe the packet was sent on.
        sync_address: u16,
        /// The source that sent the synchronization packet.
        cid: Cid,
    },
}

#[cfg(feature = "alloc")]
impl PacketOutcome<'_> {
    /// Calls `push` with the owned [`BasicReceiverEvent`] form of each event this
    /// outcome carries, in emission order.
    pub fn for_each_owned(&self, mut push: impl FnMut(BasicReceiverEvent)) {
        match self {
            PacketOutcome::Ignored => {}
            PacketOutcome::LimitExceeded { universe } => {
                push(BasicReceiverEvent::SourceLimitExceeded {
                    universe: *universe,
                });
            }
            PacketOutcome::Data {
                universe,
                data,
                pap_lost,
            } => {
                if let Some(source) = pap_lost {
                    push(BasicReceiverEvent::SourcePapLost {
                        universe: *universe,
                        source: source.to_owned(),
                    });
                }
                if let Some(data) = data {
                    push(BasicReceiverEvent::UniverseData(data.to_owned()));
                }
            }
            PacketOutcome::Sync { sync_address, cid } => {
                push(BasicReceiverEvent::SyncReceived {
                    sync_address: *sync_address,
                    cid: *cid,
                });
            }
        }
    }
}

// --- Poll events -------------------------------------------------------------

/// A poll notification emitted by a [`BasicReceiver`](super::BasicReceiver).
///
/// Borrows the receiver's reusable loss scratch, so it is valid only until the
/// next [`PollOutcome::next_event`](super::PollOutcome::next_event) call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BasicReceiverPollEvent<'a> {
    /// A sampling period ended for a universe; level data received from all
    /// sources on this universe should now be reconciled and acted upon.
    SamplingEnded {
        /// The universe whose sampling period ended.
        universe: Universe,
    },
    /// One or more sources stopped transmitting on a universe (by timing out or
    /// by sending a terminated stream). Sources lost in quick succession are
    /// grouped into a single notification so the application can react to them
    /// together.
    SourcesLost {
        /// The universe the sources were lost on.
        universe: Universe,
        /// The sources that were lost.
        sources: &'a [LostSource],
    },
}

/// The outcome of a [`poll`](BasicReceiver::poll) call: the next timer deadline
/// plus a lazily-drained sequence of the events the poll produced.
///
/// The events are drawn out one at a time with [`next_event`](Self::next_event);
/// each borrows the receiver's reusable loss scratch and is invalidated by the
/// next call. Always drain all events from this struct, even if you don't handle
/// them!
#[derive(Debug)]
pub struct PollOutcome<'a, S: BasicReceiverStorage> {
    /// The earliest instant at which calling `poll` again could produce a
    /// different result (a timer deadline), or `None` if nothing is pending.
    pub deadline: Option<Instant>,
    receiver: &'a mut BasicReceiver<S>,
    now: Instant,
    polled_univ_index: usize,
    state: UniversePollPhase,
}

impl<'a, S: BasicReceiverStorage> PollOutcome<'a, S> {
    pub(super) fn new(
        deadline: Option<Instant>,
        receiver: &'a mut BasicReceiver<S>,
        now: Instant,
    ) -> Self {
        Self {
            deadline,
            receiver,
            now,
            polled_univ_index: 0,
            state: UniversePollPhase::SamplingEnded,
        }
    }

    /// The next event this poll produced, or `None` once every listened universe
    /// has been drained.
    pub fn next_event(&mut self) -> Option<BasicReceiverPollEvent<'_>> {
        match self.advance()? {
            Emit::SamplingEnded(universe) => {
                Some(BasicReceiverPollEvent::SamplingEnded { universe })
            }
            Emit::SourcesLost(universe) => Some(BasicReceiverPollEvent::SourcesLost {
                universe,
                sources: self.receiver.lost_sources(),
            }),
        }
    }

    /// Walks universes until one owes an event, applies that event's committing
    /// mutation, and reports which event to deliver. Returns `None` once every
    /// universe is drained.
    fn advance(&mut self) -> Option<Emit> {
        loop {
            let &universe = self.receiver.polled_universe(self.polled_univ_index)?;
            match self.state {
                // End the sampling period and emit, if it is due.
                UniversePollPhase::SamplingEnded => {
                    self.state = UniversePollPhase::SourcesLost;
                    if self.receiver.maybe_end_sampling_period(&universe, self.now) {
                        return Some(Emit::SamplingEnded(universe));
                    }
                }
                // Fire any settled termination set and emit its lost sources.
                UniversePollPhase::SourcesLost => {
                    self.state = UniversePollPhase::Done;
                    if self.receiver.maybe_fire_lost_sources(&universe, self.now) {
                        return Some(Emit::SourcesLost(universe));
                    }
                }
                // This universe is drained; advance to the next.
                _ => {
                    self.polled_univ_index += 1;
                    self.state = UniversePollPhase::SamplingEnded;
                }
            }
        }
    }
}

/// Which event a universe owes, for [`PollOutcome::advance`].
enum Emit {
    SamplingEnded(Universe),
    SourcesLost(Universe),
}

/// State tracking for [`PollOutcome::advance`].
#[derive(Debug)]
enum UniversePollPhase {
    SamplingEnded,
    SourcesLost,
    Done,
}
