//! Error types for the core.
//!
//! These types are `no_std`-compatible (they target [`core::error::Error`] via
//! `thiserror` with `default-features = false`).
//!
//! Anything that needs [`std::io::Error`] lives in the adapter layer, not here.

/// The core library error type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum Error {
    /// A packet failed to parse or serialize. See [`CodecError`].
    #[error(transparent)]
    Codec(#[from] CodecError),

    /// A universe number was outside the valid range
    /// [`Universe::MIN`]`..=`[`Universe::MAX`].
    ///
    /// [`Universe::MIN`]: crate::Universe::MIN
    /// [`Universe::MAX`]: crate::Universe::MAX
    #[error("universe {value} is out of the valid range 1..=63999")]
    InvalidUniverse {
        /// The rejected universe number.
        value: u16,
    },

    /// A priority was greater than [`Priority::MAX`].
    ///
    /// [`Priority::MAX`]: crate::Priority::MAX
    #[error("priority {value} exceeds the maximum of 200")]
    InvalidPriority {
        /// The rejected priority value.
        value: u8,
    },

    /// An ad-hoc send was attempted for a START code a source manages itself
    /// (`0x00` NULL levels or `0xDD` per-address priority). Use the level and
    /// per-address-priority update methods for those instead.
    #[error("start code {start_code:#04x} is managed by the source and cannot be sent ad hoc")]
    ReservedStartCode {
        /// The rejected START code.
        start_code: u8,
    },

    /// An operation referenced a universe that was not previously registered.
    #[error("universe {universe} not found")]
    NoSuchUniverse {
        /// The universe that was not found.
        universe: u16,
    },

    /// A requested operation hit a capacity limitation
    #[error("no capacity to perform the requested operation")]
    NoCapacity,
}

/// The core library result type.
pub type Result<T> = core::result::Result<T, Error>;

/// An error encountered while parsing or serializing an E1.31 PDU.
///
/// Carries the byte [`offset`](CodecError::offset) at which the failure was
/// detected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[error("codec error at byte offset {offset}: {kind}")]
#[non_exhaustive]
pub struct CodecError {
    /// Byte offset into the packet at which the error was detected.
    pub offset: usize,
    /// What specifically went wrong.
    pub kind: CodecErrorKind,
}

/// The specific failure described by a [`CodecError`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum CodecErrorKind {
    /// The input ended before a complete PDU could be read.
    #[error("unexpected end of input: needed {needed} more bytes")]
    UnexpectedEof {
        /// Number of additional bytes that were required.
        needed: usize,
    },

    /// A declared PDU length was inconsistent with the buffer.
    #[error("declared length {declared} is inconsistent with the available {available} bytes")]
    InvalidLength {
        /// The length declared in the PDU's flags-and-length field.
        declared: usize,
        /// The number of bytes actually available.
        available: usize,
    },

    /// The ACN packet identifier / preamble did not match the expected bytes.
    #[error("invalid ACN preamble")]
    InvalidPreamble,

    /// A vector field held a value this implementation does not recognise.
    #[error("unknown {layer} vector {value:#x}")]
    UnknownVector {
        /// The protocol layer whose vector was unrecognised.
        layer: VectorLayer,
        /// The unrecognised vector value (widened to `u32`).
        value: u32,
    },

    /// A data packet's DMP layer had one of its fixed fields set to an
    /// unexpected value (address/data type, first property address or address
    /// increment).
    #[error("malformed DMP layer")]
    MalformedDmpLayer,

    /// A data packet's DMP property value count was zero; it must be at least 1
    /// to carry the START code.
    #[error("DMP property value count is zero (no START code present)")]
    EmptyDmpLayer,

    /// A packet carried more values than the protocol permits (512 DMX slots per
    /// data packet, 512 universes per discovery page). Reported both when
    /// parsing such a packet and when attempting to serialize one.
    #[error("{count} values exceeds the maximum of {max}")]
    TooManyValues {
        /// The number of values found or supplied.
        count: usize,
        /// The maximum the protocol permits.
        max: usize,
    },

    /// The output buffer supplied for serialization was too small.
    #[error("output buffer too small: needed {needed} more bytes, {available} available")]
    BufferTooSmall {
        /// Number of additional bytes that were required.
        needed: usize,
        /// Number of bytes still available in the output buffer.
        available: usize,
    },
}

/// The protocol layer a [`CodecErrorKind::UnknownVector`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum VectorLayer {
    /// The ACN root layer.
    Root,
    /// The E1.31 framing layer.
    Framing,
    /// The DMP (Device Management Protocol) layer.
    Dmp,
    /// The E1.31 universe discovery layer.
    UniverseDiscovery,
}

impl core::fmt::Display for VectorLayer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::Root => "root",
            Self::Framing => "framing",
            Self::Dmp => "DMP",
            Self::UniverseDiscovery => "universe discovery",
        };
        f.write_str(s)
    }
}
