//! Notifications and outcomes produced by the
//! [`SourceDetector`](super::SourceDetector).

#[cfg(feature = "alloc")]
use alloc::string::{String, ToString};
#[cfg(feature = "alloc")]
use alloc::vec::Vec;

use crate::time::Instant;
use crate::types::{Cid, SourceName};

// --- Owned event used by adapter layers -------------------------------------

/// A notification emitted by a [`SourceDetector`](super::SourceDetector).
///
/// This is the owned form, gathering the events produced by
/// [`handle_packet`](super::SourceDetector::handle_packet) and
/// [`poll`](super::SourceDetector::poll) into one enum. It is typically
/// constructed and emitted by adapter layers.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceDetectorEvent {
    /// A source was newly discovered, or an already-known source changed the
    /// list of universes it advertises. Carries the source's current, complete
    /// universe list.
    ///
    /// There is no separate "new source" event: the first `SourceUpdated` for
    /// a source announces its discovery.
    SourceUpdated {
        /// The source's Component Identifier.
        cid: Cid,
        /// The source's human-readable name (empty if it sent an invalid one).
        name: String,
        /// The universes the source is currently transmitting, in ascending
        /// order. These are the raw values advertised by the source; conformant
        /// sources send `1..=63999`, but this is not validated, so it can be any
        /// 16-bit value. Empty if the source advertises no universes.
        universes: Vec<u16>,
    },

    /// A source stopped sending universe discovery packets for long enough to be
    /// considered gone.
    SourceExpired {
        /// The expired source's Component Identifier.
        cid: Cid,
        /// The expired source's last known name.
        name: String,
    },

    /// The source limit was reached, so a newly seen source could not be tracked.
    ///
    /// Rate-limited: emitted once when the limit is first hit and not again until
    /// a tracked source expires and the limit is reached again.
    SourceLimitExceeded,

    /// A tracked source advertised more universes than the per-source limit, so
    /// its list was truncated.
    ///
    /// Rate-limited per source: emitted once when a source first overflows and
    /// not again until its universe count drops below the limit and reaches it
    /// again.
    UniverseLimitExceeded {
        /// The source whose advertised universe list was truncated.
        cid: Cid,
    },
}

// --- Borrowed packet outcome -------------------------------------------------

/// A source update, borrowing the source's name and universe list from the
/// [`SourceDetector`](super::SourceDetector)'s own storage.
///
/// The universe list is reassembled across discovery pages and lives inside the
/// detector, so (unlike the receiver's per-packet outcomes) this borrows the
/// detector rather than the packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceUpdateRef<'a> {
    /// The source's Component Identifier.
    pub cid: Cid,
    /// The source's human-readable name.
    pub name: &'a str,
    /// The source's current universe list, in ascending order.
    pub universes: &'a [u16],
}

#[cfg(feature = "alloc")]
impl SourceUpdateRef<'_> {
    /// Copies this into an owned [`SourceDetectorEvent::SourceUpdated`].
    #[must_use]
    pub fn to_owned(&self) -> SourceDetectorEvent {
        SourceDetectorEvent::SourceUpdated {
            cid: self.cid,
            name: self.name.to_string(),
            universes: self.universes.to_vec(),
        }
    }
}

/// Which configured limit a [`SourceDetector`](super::SourceDetector) hit while
/// handling a packet.
///
/// At most one of these can occur per packet: reaching the source limit rejects
/// the packet's source before any of its universes are processed, so the
/// universe limit cannot also fire in the same call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitExceeded {
    /// The source limit was reached, so a new source could not be tracked.
    ///
    /// Suppressed detector-wide until a tracked source expires and the limit is
    /// reached again, so this deliberately does not name the rejected source:
    /// many distinct sources may be turned away while a single notification
    /// stands.
    Source,

    /// A tracked source advertised more universes than the per-source limit, so
    /// its list was truncated.
    ///
    /// Suppressed per source until that source's universe count drops below the
    /// limit and reaches it again.
    Universe {
        /// The source whose advertised universe list was truncated.
        cid: Cid,
    },
}

/// The events produced by a single
/// [`handle_packet`](super::SourceDetector::handle_packet) call.
///
/// A discovery packet can, at most, report that a new source could not be
/// tracked and complete a changed universe list, so the whole possibility space
/// is captured by these two fields. The update borrows the detector's storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DetectorPacketOutcome<'a> {
    /// The source update produced, if this packet completed a changed universe
    /// list for its source.
    pub updated: Option<SourceUpdateRef<'a>>,
    /// The limit hit while handling this packet, if any: the source limit (a new
    /// source could not be tracked) or the per-source universe limit (a source's
    /// advertised list was truncated). At most one can occur per packet.
    pub limit_exceeded: Option<LimitExceeded>,
}

impl DetectorPacketOutcome<'_> {
    /// An outcome that produced nothing.
    pub(super) const IGNORED: Self = Self {
        updated: None,
        limit_exceeded: None,
    };

    /// Calls `push` with the owned [`SourceDetectorEvent`] form of each event
    /// this outcome carries.
    #[cfg(feature = "alloc")]
    pub fn for_each_owned(&self, mut push: impl FnMut(SourceDetectorEvent)) {
        match self.limit_exceeded {
            Some(LimitExceeded::Source) => push(SourceDetectorEvent::SourceLimitExceeded),
            Some(LimitExceeded::Universe { cid }) => {
                push(SourceDetectorEvent::UniverseLimitExceeded { cid });
            }
            None => {}
        }
        if let Some(updated) = &self.updated {
            push(updated.to_owned());
        }
    }
}

// --- Poll outcome ------------------------------------------------------------

/// A poll notification emitted by a [`SourceDetector`](super::SourceDetector).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceDetectorPollEvent {
    /// A source stopped sending universe discovery packets for long enough to be
    /// considered gone.
    SourceExpired {
        /// The expired source's Component Identifier.
        cid: Cid,
        /// The expired source's last known name.
        name: SourceName,
    },
}

#[cfg(feature = "alloc")]
impl From<&SourceDetectorPollEvent> for SourceDetectorEvent {
    fn from(value: &SourceDetectorPollEvent) -> Self {
        match value {
            SourceDetectorPollEvent::SourceExpired { cid, name } => {
                SourceDetectorEvent::SourceExpired {
                    cid: *cid,
                    name: name.as_str().to_string(),
                }
            }
        }
    }
}

/// The outcome of a [`poll`](super::SourceDetector::poll) call: the next timer
/// deadline plus the events it produced.
#[derive(Debug)]
pub struct DetectorPollOutcome<'a> {
    /// The earliest instant at which calling `poll` again could produce a
    /// different result (the next source-expiry deadline), or `None` if no
    /// sources are being tracked.
    pub deadline: Option<Instant>,
    events: &'a [SourceDetectorPollEvent],
}

impl<'a> DetectorPollOutcome<'a> {
    pub(super) fn new(deadline: Option<Instant>, events: &'a [SourceDetectorPollEvent]) -> Self {
        Self { deadline, events }
    }

    /// The events produced by this `poll`, in emission order. Borrowed from the
    /// detector's reusable scratch; valid until the next `poll`.
    #[must_use]
    pub fn events(&self) -> &[SourceDetectorPollEvent] {
        self.events
    }
}
