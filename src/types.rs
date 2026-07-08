//! Core protocol types.

use core::fmt;

use crate::error::{Error, Result};

/// The maximum number of bytes an E1.31 source name can hold.
const SOURCE_NAME_CAP: usize = 64;

/// A source's human-readable name: an inline, fixed-capacity UTF-8 string.
///
/// Constructing one from a longer string truncates it at a UTF-8 character
/// boundary.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SourceName(heapless::String<SOURCE_NAME_CAP>);

impl SourceName {
    /// An empty name.
    pub const fn new() -> Self {
        Self(heapless::String::new())
    }

    /// Builds a name from `name`, truncating at a character boundary if it
    /// exceeds the protocol maximum.
    pub fn from_str_lossy(name: &str) -> Self {
        let mut this = Self::new();
        this.set(name);
        this
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Whether the name is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Replaces the contents with `name`, truncating at a character boundary if
    /// it exceeds the protocol maximum.
    pub fn set(&mut self, name: &str) {
        self.0.clear();
        for ch in name.chars() {
            if self.0.push(ch).is_err() {
                break;
            }
        }
    }
}

impl core::ops::Deref for SourceName {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for SourceName {
    fn from(name: &str) -> Self {
        Self::from_str_lossy(name)
    }
}

impl PartialEq<str> for SourceName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SourceName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl fmt::Debug for SourceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for SourceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Component Identifier (CID): the RFC 4122 UUID that uniquely identifies an
/// ACN component (a source or receiver).
///
/// With the `uuid` feature enabled, [`From`] conversions to and from
/// `uuid::Uuid` are available.
///
/// `Cid` is totally ordered (by its raw bytes) so it can be used as a map key
/// when tracking sources.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cid([u8; 16]);

impl Cid {
    /// Constructs a `Cid` from its 16 raw bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the 16 raw bytes of this `Cid`.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Consumes the `Cid`, returning its 16 raw bytes.
    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Debug for Cid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render as a canonical 8-4-4-4-12 UUID string.
        let b = &self.0;
        write!(f, "Cid(")?;
        for (i, byte) in b.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

#[cfg(feature = "uuid")]
impl From<uuid::Uuid> for Cid {
    fn from(uuid: uuid::Uuid) -> Self {
        Self(*uuid.as_bytes())
    }
}

#[cfg(feature = "uuid")]
impl From<Cid> for uuid::Uuid {
    fn from(cid: Cid) -> Self {
        uuid::Uuid::from_bytes(cid.0)
    }
}

/// A sACN universe number.
///
/// Valid universes are in the range [`Universe::MIN`]`..=`[`Universe::MAX`]
/// (`1..=63999`); `0` and the discovery range above `63999` are excluded.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Universe(u16);

impl Universe {
    /// The lowest valid universe number (`1`).
    pub const MIN: u16 = 1;
    /// The highest valid universe number (`63999`).
    pub const MAX: u16 = 63999;

    /// Constructs a `Universe`, validating that `value` is in range.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidUniverse`] if `value` is outside
    /// [`Universe::MIN`]`..=`[`Universe::MAX`].
    pub const fn new(value: u16) -> Result<Self> {
        if value < Self::MIN || value > Self::MAX {
            Err(Error::InvalidUniverse { value })
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the underlying universe number.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Universe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A sACN data priority (E1.31 §6.2.3).
///
/// Valid priorities are `0..=`[`Priority::MAX`] (`0..=200`). The protocol
/// default, used when a source does not specify one, is [`Priority::DEFAULT`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Priority(u8);

impl Priority {
    /// The highest valid priority (`200`).
    pub const MAX: u8 = 200;
    /// The protocol default priority (`100`).
    pub const DEFAULT: Priority = Priority(100);

    /// Constructs a `Priority`, validating that `value` does not exceed
    /// [`Priority::MAX`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPriority`] if `value > 200`.
    pub const fn new(value: u8) -> Result<Self> {
        if value > Self::MAX {
            Err(Error::InvalidPriority { value })
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the underlying priority value.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A DMX START code: the first slot of a data packet's payload, identifying
/// what the remaining slots mean.
///
/// Every `u8` is a valid START code. When working with [`Source`], two START
/// codes are considered [reserved](StartCode::is_reserved) because of the
/// special semantics that E1.31 requires when sending them: [`NULL`](Self::NULL)
/// (`0x00`), DMX levels, and [`PAP`](Self::PAP) (`0xDD`), per-address priority.
///
/// [`Source`]: crate::source::Source
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StartCode(u8);

impl StartCode {
    /// The NULL START code (`0x00`): DMX512 levels. A source manages these
    /// itself with transmission suppression; they cannot be sent ad hoc.
    pub const NULL: StartCode = StartCode(0x00);

    /// The per-address priority START code (`0xDD`): a priority per slot. A
    /// source manages these itself; they cannot be sent ad hoc.
    pub const PAP: StartCode = StartCode(0xDD);

    /// Constructs a `StartCode` from its raw value.
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the underlying START code value.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Whether this START code is one a [`Source`](crate::source::Source)
    /// manages on its own schedule ([`NULL`](Self::NULL) or [`PAP`](Self::PAP)),
    /// and so may not be sent ad hoc.
    pub const fn is_reserved(self) -> bool {
        matches!(self, Self::NULL | Self::PAP)
    }
}

impl fmt::Display for StartCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#04x}", self.0)
    }
}

/// An E1.31 packet sequence number (§6.7.2).
///
/// Sequence numbers wrap around the full `u8` range; every value is valid.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SequenceNumber(u8);

impl SequenceNumber {
    /// Constructs a `SequenceNumber` from a raw value.
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the underlying value.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Returns the next sequence number, wrapping on overflow.
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// Returns whether `self` should be accepted as following `previous` under
    /// E1.31's sequence-number algorithm (§6.7.2).
    ///
    /// A packet is accepted if its sequence number is newer than the last
    /// accepted one, or far enough behind to indicate a wrap (or a source
    /// restart) rather than a stale duplicate. The comparison is done modulo
    /// 256.
    pub(crate) const fn supersedes(self, previous: SequenceNumber) -> bool {
        let diff = self.0 as i16 - previous.0 as i16;
        diff > 0 || diff <= -20
    }
}

impl fmt::Display for SequenceNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An opaque handle identifying a network interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NetintId(u32);

impl NetintId {
    /// A placeholder handle for adapters that do not yet attribute received
    /// packets to a specific interface.
    pub const UNKNOWN: NetintId = NetintId(0);

    /// Creates a handle from an adapter-assigned identifier.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The raw adapter-assigned identifier.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for NetintId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn universe_validates_range() {
        assert!(matches!(
            Universe::new(0),
            Err(Error::InvalidUniverse { value: 0 })
        ));
        assert!(matches!(
            Universe::new(64000),
            Err(Error::InvalidUniverse { value: 64000 })
        ));
        assert_eq!(Universe::new(1).unwrap().get(), 1);
        assert_eq!(Universe::new(63999).unwrap().get(), 63999);
    }

    #[test]
    fn priority_validates_range_and_default() {
        assert!(matches!(
            Priority::new(201),
            Err(Error::InvalidPriority { value: 201 })
        ));
        assert_eq!(Priority::new(200).unwrap().get(), 200);
        assert_eq!(Priority::new(0).unwrap().get(), 0);
        assert_eq!(Priority::default().get(), 100);
    }

    #[test]
    fn sequence_number_wraps() {
        assert_eq!(SequenceNumber::new(255).next().get(), 0);
        assert_eq!(SequenceNumber::new(7).next().get(), 8);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn sequence_number_supersedes_newer_and_wrap() {
        let seq = SequenceNumber::new;
        assert!(seq(1).supersedes(seq(0)));
        assert!(seq(10).supersedes(seq(5)));
        // Equal or slightly behind: stale, reject.
        assert!(!seq(0).supersedes(seq(0)));
        assert!(!seq(5).supersedes(seq(10)));
        // Far behind: treat as a wrap/restart and accept.
        assert!(seq(0).supersedes(seq(255)));
        assert!(seq(5).supersedes(seq(30)));
    }

    #[test]
    fn cid_round_trips_bytes() {
        let bytes = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let cid = Cid::from_bytes(bytes);
        assert_eq!(cid.as_bytes(), &bytes);
        assert_eq!(cid.into_bytes(), bytes);
    }
}
