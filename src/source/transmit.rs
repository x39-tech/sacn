//! The outbound side of the source: the packets it emits and where they go.
//!
//! Driving [`Source::poll`](super::Source::poll) produces a [`SourcePoll`],
//! which yields the [`Transmission`]s that are due to be sent right now one
//! at a time, each pairing the serialized packet bytes with the abstract
//! [`Route`] describing where they go. Callers are responsible for owning the
//! socket, expanding the route into concrete destinations (e.g. multicast
//! groups on each configured interface, plus unicast addresses), and
//! performing the sends.
//!
//! Transmissions are serialized lazily, one at a time, into a single packet-sized
//! buffer held inside the source. The poll records only a lightweight description
//! of each packet (which universe, which kind, the sequence number); the bytes
//! are produced on demand as [`SourcePoll::next_transmission`] walks the queue.

use crate::storage::VecLike;
use crate::time::Instant;
use crate::types::Universe;

use super::{Source, SourceStorage};

/// Where a single [`Transmission`] must be delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Route {
    /// A universe's data packet. Send to that universe's multicast group on
    /// each interface configured for the universe, and to each of the
    /// universe's unicast destinations, if applicable.
    Universe(Universe),
    /// A universe-discovery page. Send to the reserved discovery multicast
    /// group on the interfaces being transmitted on.
    Discovery,
    /// A synchronization packet for a synchronization universe. Send to that
    /// universe's multicast group on the union of the sync group members'
    /// interfaces, plus the union of the members' unicast destinations.
    Sync(Universe),
}

/// A single packet ready to be sent: the serialized bytes and where they go.
///
/// The [`data`](Self::data) slice borrows the single packet buffer held inside
/// the [`Source`](super::Source); it is valid only until the next call to
/// [`SourcePoll::next_transmission`], which reuses that buffer for the following
/// packet.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Transmission<'a> {
    /// The destination the bytes must be sent to.
    pub route: Route,
    /// The serialized sACN packet to send.
    pub data: &'a [u8],
}

/// The outcome of a [`poll`](super::Source::poll): the next instant at which
/// polling could produce more, plus the transmissions due to be sent now,
/// drained one at a time with [`next_transmission`](Self::next_transmission).
///
/// Holding a `SourcePoll` borrows the source mutably, so the queued
/// transmissions must be drained before the source can be polled or mutated
/// again. Each transmission is serialized on demand into a buffer shared across
/// the whole drain, so only one [`Transmission`] is live at a time.
///
/// After a transmission is drained, the caller is responsible for delivering
/// it to any destinations that it is configured to go to. Tracking state for
/// partial delivery to a packet's destinations is the responsibility of the
/// caller.
#[derive(Debug)]
pub struct SourcePoll<'a, S: SourceStorage = crate::storage::HeapStorage> {
    /// The earliest instant at which calling [`poll`](super::Source::poll) again
    /// could produce another transmission (a keep-alive, the next pre-suppression
    /// packet, the next termination packet, or the next discovery announcement),
    /// or `None` if the source has nothing to send and no timers pending.
    ///
    /// Calling `poll` earlier is harmless (it simply finds nothing due); calling
    /// it later only delays transmissions.
    pub deadline: Option<Instant>,
    source: &'a mut Source<S>,
}

impl<'a, S: SourceStorage> SourcePoll<'a, S> {
    pub(super) fn new(deadline: Option<Instant>, source: &'a mut Source<S>) -> Self {
        Self { deadline, source }
    }

    /// Serializes and returns the next transmission due this poll, advancing the
    /// drain past it, or `None` once they are exhausted.
    ///
    /// Each call reuses a single packet buffer inside the source, so the returned
    /// [`Transmission`] borrows that buffer and is invalidated by the next call
    /// (or by [`send_now`](super::Source::send_now)). Send (or copy) each
    /// transmission before requesting the next.
    ///
    /// The just-returned bytes remain readable via
    /// [`current_packet`](super::Source::current_packet) until the next
    /// serialization.
    pub fn next_transmission(&mut self) -> Option<Transmission<'_>> {
        if self.source.cursor >= self.source.pending.len() {
            return None;
        }
        let idx = self.source.cursor;
        self.source.cursor += 1;
        Some(self.source.serialize_at(idx))
    }

    /// The universes physically dropped by this poll, because their termination
    /// sequence completed on the previous poll.
    ///
    /// This indicates that the source is no longer tracking these universes
    /// and will no longer send any data to them unless they are added again.
    pub fn removed(&self) -> &[Universe] {
        self.source.removed.as_slice()
    }
}
