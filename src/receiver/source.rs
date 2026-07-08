//! Per-source state tracking for the basic receiver.
//!
//! A [`TrackedSource`] records, for one source on one universe, the timers and
//! the per-address-priority (PAP) state machine. The receiver feeds it packets
//! and ticks; it answers with whether the data should be forwarded and whether
//! a PAP-lost condition arose.

use crate::time::{Duration, Instant};
use crate::types::SequenceNumber;

use super::SOURCE_LOSS_TIMEOUT;

/// Where a source sits in the per-address-priority handshake.
///
/// This tracks *only* the interaction between NULL-START-code (levels) and
/// `0xDD` (per-address priority) data within a single source. It does not affect
/// source tracking or the source-loss algorithm, which treat a source uniformly
/// regardless of START code (see [`TrackedSource::ever_delivered`]).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvState {
    /// Tracked, but no NULL or PAP data has been received yet. Only reachable for
    /// a source first seen via some other (allow-listed) START code.
    Initial,
    /// The first NULL data was received with no prior PAP; its levels are
    /// withheld from the application until a `0xDD` packet arrives or the wait
    /// elapses. Only entered when PAP handling is enabled and not in a sampling
    /// period.
    WaitingForPap,
    /// The source sends levels with no PAP (the PAP wait elapsed without any
    /// `0xDD`, PAP handling is off, or it was first seen during sampling).
    HaveDmxOnly,
    /// The source has sent PAP but no levels yet.
    HavePapOnly,
    /// The source sends both levels and PAP.
    HaveDmxAndPap,
}

use RecvState::{HaveDmxAndPap, HaveDmxOnly, HavePapOnly, Initial, WaitingForPap};

/// The outcome of feeding a NULL-START-code (levels) packet to a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NullOutcome {
    /// Whether to forward this data to the application.
    pub notify: bool,
    /// Whether the source just lost its per-address priority (was sending PAP,
    /// then a level packet arrived after the PAP timeout).
    pub pap_lost: bool,
}

/// One source being tracked on a universe.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct TrackedSource {
    /// The most recent accepted sequence number.
    pub seq: SequenceNumber,
    /// Set once the source's stream has been terminated; further packets from
    /// it are ignored until it is removed.
    pub terminated: bool,
    /// Whether any data packet has been accepted since the last tick. Drives the
    /// online/unknown classification used by source-loss settling.
    pub data_received_since_last_tick: bool,
    /// Whether any [`UniverseData`](super::UniverseData) has ever been delivered
    /// to the application for this source. A source that times out (or
    /// terminates) without ever having been delivered - i.e. it only ever
    /// withheld NULL data pending PAP - is dropped silently; once it has been
    /// delivered, its loss is reported. This, not [`recv_state`](Self::recv_state),
    /// governs the source-loss algorithm.
    pub ever_delivered: bool,
    /// The PAP handshake state. Affects only NULL/PAP interaction, never tracking.
    pub recv_state: RecvState,
    /// When the source is considered lost if no further data packets arrive.
    pub packet_expiry: Instant,
    /// When the per-address-priority wait/timeout elapses. Its meaning depends
    /// on [`recv_state`](Self::recv_state).
    pub pap_expiry: Instant,
}

impl TrackedSource {
    /// Creates a freshly tracked source in the [`Initial`](RecvState::Initial)
    /// state. The first packet is then fed through the same handling as any
    /// later one (via [`register_data_packet`](Self::register_data_packet) and
    /// [`process_null`](Self::process_null) / [`process_pap`](Self::process_pap)),
    /// so source creation has no special cases.
    pub(super) fn new(now: Instant, seq: SequenceNumber) -> Self {
        Self {
            seq,
            terminated: false,
            data_received_since_last_tick: true,
            ever_delivered: false,
            recv_state: Initial,
            packet_expiry: now.saturating_add(SOURCE_LOSS_TIMEOUT),
            // Unused until the source enters a PAP-tracking state.
            pap_expiry: now,
        }
    }

    /// Records that an accepted data packet arrived, refreshing the network
    /// data loss timer and marking the source as having sent this tick.
    ///
    /// Per E1.31 §6.7.1 this applies to every accepted packet regardless of
    /// START code (NULL, per-address priority, or any other), so the caller
    /// invokes it before the START-code-specific handling below.
    pub(super) fn register_data_packet(&mut self, now: Instant) {
        self.data_received_since_last_tick = true;
        self.packet_expiry = now.saturating_add(SOURCE_LOSS_TIMEOUT);
    }

    /// Marks the source's stream as terminated, expiring its packet timer
    /// immediately so the next tick treats it as lost.
    pub(super) fn mark_terminated(&mut self, now: Instant) {
        self.terminated = true;
        self.packet_expiry = now;
    }

    /// Processes a NULL-START-code (levels) packet's PAP state machine.
    ///
    /// The network data loss timer and the received-this-tick flag are handled
    /// by [`register_data_packet`](Self::register_data_packet); this method only
    /// drives the per-address-priority handshake specific to levels. `sampling`,
    /// `pap_handling` and `pap_wait` are needed because the *first* NULL packet
    /// (with no prior PAP) decides whether to withhold the levels pending a
    /// `0xDD` packet.
    pub(super) fn process_null(
        &mut self,
        now: Instant,
        sampling: bool,
        pap_handling: bool,
        pap_wait: Duration,
    ) -> NullOutcome {
        let mut outcome = NullOutcome {
            notify: true,
            pap_lost: false,
        };

        match self.recv_state {
            Initial => {
                // The first NULL data with no prior PAP. Outside a sampling
                // period, and with PAP handling on, withhold the levels for a
                // while in case the source also sends per-address priority.
                if pap_handling && !sampling {
                    self.recv_state = WaitingForPap;
                    self.pap_expiry = now.saturating_add(pap_wait);
                    outcome.notify = false;
                } else {
                    self.recv_state = HaveDmxOnly;
                }
            }
            HavePapOnly => self.recv_state = HaveDmxAndPap,
            WaitingForPap => {
                if now >= self.pap_expiry {
                    // The PAP wait elapsed without any 0xDD packet: fall back to
                    // packet priority. Keep a timer running in case PAP starts
                    // arriving later.
                    self.recv_state = HaveDmxOnly;
                    self.pap_expiry = now.saturating_add(SOURCE_LOSS_TIMEOUT);
                } else {
                    // Still waiting to learn whether this source sends PAP.
                    outcome.notify = false;
                }
            }
            HaveDmxOnly => {}
            HaveDmxAndPap => {
                if now >= self.pap_expiry {
                    // The source stopped sending PAP but is still sending levels.
                    outcome.pap_lost = true;
                    self.recv_state = HaveDmxOnly;
                }
            }
        }

        outcome
    }

    /// Processes a per-address-priority (`0xDD`) packet, updating the PAP timer
    /// and state machine. PAP data is always forwarded.
    pub(super) fn process_pap(&mut self, now: Instant) {
        match self.recv_state {
            Initial => self.recv_state = HavePapOnly,
            WaitingForPap | HaveDmxOnly => self.recv_state = HaveDmxAndPap,
            HaveDmxAndPap | HavePapOnly => {}
        }
        self.pap_expiry = now.saturating_add(SOURCE_LOSS_TIMEOUT);
    }
}
