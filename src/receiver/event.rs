//! Notification and outcome types shared by both receive tiers.
//!
//! These are the cross-cutting building blocks - source identity, per-source
//! universe data, and the listen/stop I/O outcomes - used by both the
//! [`BasicReceiver`](super::BasicReceiver) and the merging
//! [`Receiver`](super::Receiver). The tier-specific event enums live alongside
//! each receiver: see [`basic::event`](super::basic) and
//! [`merging::event`](super::merging).

use core::net::SocketAddr;

#[cfg(feature = "alloc")]
use alloc::string::{String, ToString};
#[cfg(feature = "alloc")]
use alloc::vec::Vec;

use crate::types::{Cid, Universe};

// --- Source identity ---------------------------------------------------------

/// Identifying information for a source, borrowing the packet it came from.
///
/// The owning counterpart is [`SourceInfo`]; obtain one with
/// [`to_owned`](Self::to_owned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceInfoRef<'a> {
    /// The source's Component Identifier.
    pub cid: Cid,
    /// The source's human-readable name (empty if it sent an invalid one).
    pub name: &'a str,
}

#[cfg(feature = "alloc")]
impl SourceInfoRef<'_> {
    /// Copies this into an owned [`SourceInfo`].
    #[must_use]
    pub fn to_owned(&self) -> SourceInfo {
        SourceInfo {
            cid: self.cid,
            name: self.name.to_string(),
        }
    }
}

/// Identifying information for a source.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceInfo {
    /// The source's Component Identifier.
    pub cid: Cid,
    /// The source's human-readable name (empty if it sent an invalid one).
    pub name: String,
}

// --- Per-source universe data ------------------------------------------------

/// Per-source universe data, borrowing the packet it was parsed from.
///
/// This is the zero-copy form returned by
/// [`handle_packet`](super::BasicReceiver::handle_packet): [`values`](Self::values)
/// and the source [`name`](SourceInfoRef::name) point straight into the caller's
/// datagram buffer. The owning counterpart is [`UniverseData`]; obtain one with
/// [`to_owned`](Self::to_owned). See [`UniverseData`] for field semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UniverseDataRef<'a> {
    /// The universe the data is for.
    pub universe: Universe,
    /// The source that sent the data.
    pub source: SourceInfoRef<'a>,
    /// The network address the packet was received from.
    pub addr: SocketAddr,
    /// The packet (universe) priority, as the raw 8-bit value from the wire.
    pub priority: u8,
    /// The DMX START code.
    pub start_code: u8,
    /// The data values after the START code, borrowed from the datagram. At most
    /// 512 bytes.
    pub values: &'a [u8],
    /// The Preview_Data flag from E1.31.
    pub preview: bool,
    /// The synchronization address declared by the packet. `0` means the data
    /// is not synchronized. Always `0` when the receiver has synchronization
    /// disabled.
    pub sync_address: u16,
    /// Whether this source is currently part of a sampling period.
    pub is_sampling: bool,
}

#[cfg(feature = "alloc")]
impl UniverseDataRef<'_> {
    /// Copies this into an owned [`UniverseData`], allocating for the values and
    /// source name.
    #[must_use]
    pub fn to_owned(&self) -> UniverseData {
        UniverseData {
            universe: self.universe,
            source: self.source.to_owned(),
            addr: self.addr,
            priority: self.priority,
            start_code: self.start_code,
            values: self.values.to_vec(),
            preview: self.preview,
            sync_address: self.sync_address,
            is_sampling: self.is_sampling,
        }
    }
}

/// Per-source universe data forwarded to the application.
///
/// Represents the universe data from exactly one source; merging across
/// sources is the job of a higher receive layer.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UniverseData {
    /// The universe the data is for.
    pub universe: Universe,
    /// The source that sent the data.
    pub source: SourceInfo,
    /// The network address the packet was received from.
    pub addr: SocketAddr,
    /// The packet (universe) priority. Conformant sources send `0..=200`, but
    /// this is the raw value from the wire and is not validated, so it can be
    /// any 8-bit value.
    pub priority: u8,
    /// The DMX START code. A `0x00` START code carries DMX levels in
    /// [`values`](Self::values); `0xDD` carries per-address priorities; and
    /// other alternate start codes are possible as well, see
    /// [ESTA's alternate start code list](https://tsp.esta.org/tsp/working_groups/CP/DMXAlternateCodes.php).
    pub start_code: u8,
    /// The data values after the START code. At most 512 bytes.
    pub values: Vec<u8>,
    /// The Preview_Data flag from E1.31: "indicates that the data in this
    /// packet is intended for use in visualization or media server preview
    /// applications and shall not be used to generate live output."
    pub preview: bool,
    /// The synchronization address declared by the packet. `0` means the data
    /// is not synchronized. Always `0` when the receiver has synchronization
    /// disabled.
    pub sync_address: u16,
    /// Whether this source is currently part of a sampling period. While `true`,
    /// the data should be treated as provisional.
    pub is_sampling: bool,
}

// --- Listen / stop outcomes --------------------------------------------------

/// The outcome of a [`listen`](super::BasicReceiver::listen) call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListenOutcome {
    /// `true` if a new sampling period was opened (the universe was not
    /// already being listened to); `false` on a re-listen.
    pub sampling_started: bool,
}

impl ListenOutcome {
    pub(super) fn new(sampling_started: bool) -> Self {
        Self { sampling_started }
    }
}

/// The outcome of a [`stop_listening`](super::BasicReceiver::stop_listening)
/// call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StopOutcome {
    /// Whether the universe was being listened to.
    pub was_listening: bool,
}

impl StopOutcome {
    pub(super) fn new(was_listening: bool) -> Self {
        Self { was_listening }
    }
}
