//! The E1.31 protocol codec: parsing and serialization of all sACN PDUs.
//!
//! sACN packets are a stack of nested PDUs - an ACN root layer wrapping an
//! E1.31 framing layer, which (for data packets) wraps a DMP layer.
//!
//! Three framing-layer PDU types are supported, selected by the root and
//! framing vectors:
//!
//! - [`DataPacket`] - the DMX/alternate-start-code data stream.
//! - [`SyncPacket`] - the universe synchronization packet.
//! - [`UniverseDiscoveryPacket`] - the periodic announcement of the universes a
//!   source is transmitting.
//!
//! # Parsing
//!
//! ```
//! use sacn::packet::{Packet, Payload};
//!
//! # fn demo(bytes: &[u8]) -> Result<(), sacn::error::CodecError> {
//! let packet = Packet::parse(bytes)?;
//! match packet.payload {
//!     Payload::Data(data) => { /* data.universe, data.values, ... */ }
//!     Payload::Sync(_) | Payload::UniverseDiscovery(_) => {}
//!     _ => {}
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Serializing
//!
//! Serialization writes into a caller-provided buffer (no allocation), or, with
//! the `alloc` feature, into a freshly allocated `Vec` via
//! [`Packet::to_vec`]. [`MAX_PACKET_SIZE`] bounds the size of any packet this
//! codec produces.

mod cursor;

use cursor::{Reader, Writer};

use crate::error::{CodecError, CodecErrorKind, VectorLayer};
use crate::types::{Cid, SequenceNumber};

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

// --- Wire constants ---------------------------------------------------------

/// The fixed 16-byte ACN root-layer preamble
const ACN_PREAMBLE: [u8; 16] = [
    0x00, 0x10, 0x00, 0x00, b'A', b'S', b'C', b'-', b'E', b'1', b'.', b'1', b'7', 0x00, 0x00, 0x00,
];

const VECTOR_ROOT_E131_DATA: u32 = 0x0000_0004;
const VECTOR_ROOT_E131_EXTENDED: u32 = 0x0000_0008;
pub(crate) const VECTOR_ROOT_E131_PATHWAY_SECURE: u32 = 0x5043_0001;

const VECTOR_E131_DATA_PACKET: u32 = 0x0000_0002;
const VECTOR_E131_EXTENDED_SYNCHRONIZATION: u32 = 0x0000_0001;
const VECTOR_E131_EXTENDED_DISCOVERY: u32 = 0x0000_0002;

const VECTOR_UNIVERSE_DISCOVERY_UNIVERSE_LIST: u32 = 0x0000_0001;

const VECTOR_DMP_SET_PROPERTY: u8 = 0x02;
const DMP_ADDRESS_DATA_TYPE: u8 = 0xa1;
const DMP_FIRST_PROPERTY_ADDRESS: u16 = 0x0000;
const DMP_ADDRESS_INCREMENT: u16 = 0x0001;

const OPT_PREVIEW: u8 = 0x80;
const OPT_TERMINATED: u8 = 0x40;
const OPT_FORCE_SYNC: u8 = 0x20;

/// Length of the fixed source-name field on the wire (E1.31 §6.2).
const SOURCE_NAME_LEN: usize = 64;

/// Offset of the root-layer flags-and-length field (i.e. the end of the
/// preamble); also the size of the root layer's framing-independent header.
const ROOT_FLAGS_LENGTH_OFFSET: usize = 16;
/// Offset at which the E1.31 framing layer begins (after the root layer).
const FRAMING_OFFSET: usize = 38;
/// Offset at which a data packet's DMP layer begins.
const DMP_OFFSET: usize = 115;
/// Size of a data packet with zero DMX slots (the full header).
const DATA_HEADER_SIZE: usize = 126;
/// Total size of a synchronization packet (it has no variable-length payload).
const SYNC_PACKET_SIZE: usize = 49;
/// Offset at which the universe discovery layer begins.
const UNIVERSE_DISCOVERY_OFFSET: usize = 112;
/// Size of a universe discovery packet carrying zero universes (the full
/// header).
const UNIVERSE_DISCOVERY_HEADER_SIZE: usize = 120;
/// Number of header bytes within the universe discovery layer itself (flags and
/// length, vector, page, last page) that precede the universe list.
const UNIVERSE_DISCOVERY_LAYER_HEADER: usize = 8;

/// The maximum number of DMX slots a single data packet may carry.
pub const MAX_SLOTS: usize = 512;

/// The maximum number of universes a single discovery page may list
/// (E1.31 §8).
pub const MAX_UNIVERSES_PER_PAGE: usize = 512;

/// The largest possible serialized sACN packet, in bytes.
///
/// This is a full universe discovery page (the largest PDU). Adapters can use it
/// to size receive and transmit buffers.
pub const MAX_PACKET_SIZE: usize = UNIVERSE_DISCOVERY_HEADER_SIZE + MAX_UNIVERSES_PER_PAGE * 2;

/// The NULL START code (`0x00`): a [`DataPacket`]'s values are DMX512 levels.
pub const DMX_NULL_START_CODE: u8 = 0x00;

/// The per-address-priority START code (`0xDD`): a [`DataPacket`]'s values are
/// per-slot priorities.
pub const PAP_START_CODE: u8 = 0xDD;

// --- Flags-and-length helpers ------------------------------------------------

/// Builds a PDU flags-and-length field: the standard `0x7` flags (V, H, D) in
/// the top nibble plus the 12-bit PDU length.
fn flags_and_length(len: usize) -> u16 {
    0x7000 | (len as u16 & 0x0fff)
}

/// Extracts the 12-bit PDU length from a flags-and-length field, ignoring the
/// flag nibble
fn pdu_length(flags_length: u16) -> usize {
    (flags_length & 0x0fff) as usize
}

fn err(offset: usize, kind: CodecErrorKind) -> CodecError {
    CodecError { offset, kind }
}

// --- Packet types ------------------------------------------------------------

/// A fully parsed sACN packet: the source's [`Cid`] plus the framing-layer
/// [`Payload`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Packet<'a> {
    /// The Component Identifier of the source that sent the packet.
    pub cid: Cid,
    /// The framing-layer payload.
    pub payload: Payload<'a>,
}

/// The framing-layer payload of a [`Packet`], selected by the root and framing
/// vectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Payload<'a> {
    /// A DMX / alternate-start-code data packet.
    Data(DataPacket<'a>),
    /// A universe synchronization packet.
    Sync(SyncPacket),
    /// A universe discovery packet.
    UniverseDiscovery(UniverseDiscoveryPacket<'a>),
}

/// A DMX (or per-address-priority) data packet.
///
/// [`values`](Self::values) holds the property values *after* the START code -
/// i.e. the DMX slots themselves; [`start_code`](Self::start_code) is exposed
/// separately. [`DMX_NULL_START_CODE`] and [`PAP_START_CODE`] are the most
/// commonly used START codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DataPacket<'a> {
    /// The human-readable source name. If the name is invalid UTF-8, this will
    /// be an empty string.
    pub source_name: &'a str,
    /// The data priority. Conformant sources transmit `0..=200`, but that is
    /// not validated at this level, so this can be any 8-bit value.
    pub priority: u8,
    /// The synchronization universe, or `0` if the packet is not synchronized.
    pub sync_address: u16,
    /// The packet sequence number.
    pub sequence_number: SequenceNumber,
    /// The preview-data flag: the data is for preview/visualization only.
    pub preview: bool,
    /// The stream-terminated flag: the source is ceasing transmission on this
    /// universe.
    pub stream_terminated: bool,
    /// The force-synchronization flag.
    pub force_sync: bool,
    /// The universe this packet carries data for. Conformant sources transmit
    /// `1..=63999`, but that is not validated at this level, so this can be
    /// any 16-bit value.
    pub universe: u16,
    /// The DMX START code.
    pub start_code: u8,
    /// The DMX slot values, excluding the START code. At most [`MAX_SLOTS`].
    pub values: &'a [u8],
}

/// A universe synchronization packet (E1.31 §6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SyncPacket {
    /// The packet sequence number.
    pub sequence_number: SequenceNumber,
    /// The synchronization universe this packet is synchronizing.
    pub sync_address: u16,
}

/// A universe discovery packet: one page of the universes a source is currently
/// transmitting (E1.31 §6.4, §8).
///
/// A source advertises its universes across [`last_page`](Self::last_page)`+ 1`
/// pages; a detector reassembles them by page number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UniverseDiscoveryPacket<'a> {
    /// The human-readable source name. If the name is invalid UTF-8, this will
    /// be an empty string.
    pub source_name: &'a str,
    /// The zero-based index of this page.
    pub page: u8,
    /// The zero-based index of the final page (so there are `last_page + 1`
    /// pages in total).
    pub last_page: u8,
    /// The universes listed on this page.
    pub universes: UniverseList<'a>,
}

/// The list of universes carried by a [`UniverseDiscoveryPacket`].
///
/// This is a simple view into the raw universe list bytes with nice iteration.
/// Per E1.31, universes are sorted ascending with no duplicates, but this type
/// does not enforce that - a detector validates the reassembled list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct UniverseList<'a> {
    bytes: &'a [u8],
}

impl<'a> UniverseList<'a> {
    /// Wraps a slice of big-endian `u16` universe values.
    ///
    /// A trailing odd byte (if `bytes.len()` is odd) is ignored by
    /// [`iter`](Self::iter) and [`len`](Self::len).
    pub const fn from_bytes(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// The underlying big-endian bytes.
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The number of universes in the list.
    pub const fn len(&self) -> usize {
        self.bytes.len() / 2
    }

    /// Whether the list is empty.
    pub const fn is_empty(&self) -> bool {
        self.bytes.len() < 2
    }

    /// Iterates the universes as `u16` values.
    pub fn iter(&self) -> impl Iterator<Item = u16> + 'a + use<'a> {
        self.bytes.chunks_exact(2).map(chunk_to_u16)
    }
}

impl<'a> IntoIterator for UniverseList<'a> {
    type Item = u16;
    type IntoIter = core::iter::Map<core::slice::ChunksExact<'a, u8>, fn(&[u8]) -> u16>;

    fn into_iter(self) -> Self::IntoIter {
        self.bytes.chunks_exact(2).map(chunk_to_u16)
    }
}

fn chunk_to_u16(pair: &[u8]) -> u16 {
    u16::from_be_bytes([pair[0], pair[1]])
}

impl core::fmt::Debug for UniverseList<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

// --- Parsing -----------------------------------------------------------------

impl<'a> Packet<'a> {
    /// Parses a complete sACN packet from `buf`.
    ///
    /// # Errors
    ///
    /// Returns a [`CodecError`] (carrying the byte offset at which the failure
    /// was detected) if a basic protocol-level value is invalid such as the
    /// preamble, a vector, the DMP layer, a length field, etc., or if the
    /// buffer is shorter than the PDU it claims to contain.
    pub fn parse(buf: &'a [u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(buf);

        let preamble = r.bytes(ACN_PREAMBLE.len())?;
        // The post-amble size (bytes 2..4) is not validated: base E1.31 sets it
        // to zero, while the Pathway Secure extension sets it to 28. The preamble
        // size and the ACN packet identifier around it are fixed.
        if preamble[..2] != ACN_PREAMBLE[..2] || preamble[4..] != ACN_PREAMBLE[4..] {
            return Err(err(0, CodecErrorKind::InvalidPreamble));
        }
        // Root-layer flags-and-length is not relied upon; the per-PDU layers
        // are bounded by their own length/count fields.
        let _root_length = pdu_length(r.u16()?);
        let root_vector_offset = r.position();
        let root_vector = r.u32()?;
        let cid = Cid::from_bytes(r.array::<16>()?);

        let payload = match root_vector {
            VECTOR_ROOT_E131_DATA | VECTOR_ROOT_E131_PATHWAY_SECURE => {
                Payload::Data(parse_data(&mut r)?)
            }
            VECTOR_ROOT_E131_EXTENDED => parse_extended(&mut r)?,
            other => {
                return Err(err(
                    root_vector_offset,
                    CodecErrorKind::UnknownVector {
                        layer: VectorLayer::Root,
                        value: other,
                    },
                ));
            }
        };

        Ok(Packet { cid, payload })
    }
}

/// Parses the source-name field: the bytes up to the first NUL, decoded as
/// UTF-8.
///
/// The final byte of the 64-byte field is reserved for the NUL terminator
/// (E1.31 requires the name be null-terminated), so at most 63 bytes of name
/// are read. The standard mandates a UTF-8 name but gives receivers no rule
/// to discard a malformed one, and the name never affects data processing, so
/// an invalid-UTF-8 field degrades to an empty string rather than failing the
/// whole packet.
fn parse_source_name(field: &[u8]) -> &str {
    let searchable = &field[..SOURCE_NAME_LEN - 1];
    let end = searchable
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(searchable.len());
    core::str::from_utf8(&searchable[..end]).unwrap_or("")
}

/// Parses a data packet's framing and DMP layers. `r` is positioned at the start
/// of the framing layer.
fn parse_data<'a>(r: &mut Reader<'a>) -> Result<DataPacket<'a>, CodecError> {
    let _framing_length = pdu_length(r.u16()?);
    let framing_vector_offset = r.position();
    let framing_vector = r.u32()?;
    if framing_vector != VECTOR_E131_DATA_PACKET {
        return Err(err(
            framing_vector_offset,
            CodecErrorKind::UnknownVector {
                layer: VectorLayer::Framing,
                value: framing_vector,
            },
        ));
    }

    let source_name = parse_source_name(r.bytes(SOURCE_NAME_LEN)?);
    let priority = r.u8()?;
    let sync_address = r.u16()?;
    let sequence_number = SequenceNumber::new(r.u8()?);
    let options = r.u8()?;
    let universe = r.u16()?;

    // DMP layer.
    let _dmp_length = pdu_length(r.u16()?);
    let dmp_vector_offset = r.position();
    let dmp_vector = r.u8()?;
    if dmp_vector != VECTOR_DMP_SET_PROPERTY {
        return Err(err(
            dmp_vector_offset,
            CodecErrorKind::UnknownVector {
                layer: VectorLayer::Dmp,
                value: u32::from(dmp_vector),
            },
        ));
    }

    let dmp_fixed_offset = r.position();
    let address_data_type = r.u8()?;
    let first_property_address = r.u16()?;
    let address_increment = r.u16()?;
    if address_data_type != DMP_ADDRESS_DATA_TYPE
        || first_property_address != DMP_FIRST_PROPERTY_ADDRESS
        || address_increment != DMP_ADDRESS_INCREMENT
    {
        return Err(err(dmp_fixed_offset, CodecErrorKind::MalformedDmpLayer));
    }

    let count_offset = r.position();
    let property_value_count = r.u16()?;
    if property_value_count < 1 {
        return Err(err(count_offset, CodecErrorKind::EmptyDmpLayer));
    }
    let start_code = r.u8()?;
    // The count includes the START code; the DMX slots are the remainder. A
    // universe is at most 512 slots (E1.31 §7).
    let slot_count = usize::from(property_value_count - 1);
    if slot_count > MAX_SLOTS {
        return Err(err(
            count_offset,
            CodecErrorKind::TooManyValues {
                count: slot_count,
                max: MAX_SLOTS,
            },
        ));
    }
    let values = r.bytes(slot_count)?;

    Ok(DataPacket {
        source_name,
        priority,
        sync_address,
        sequence_number,
        preview: options & OPT_PREVIEW != 0,
        stream_terminated: options & OPT_TERMINATED != 0,
        force_sync: options & OPT_FORCE_SYNC != 0,
        universe,
        start_code,
        values,
    })
}

/// Parses an extended packet: synchronization or universe discovery, selected
/// by the framing vector. `r` is positioned at the start of the framing layer.
fn parse_extended<'a>(r: &mut Reader<'a>) -> Result<Payload<'a>, CodecError> {
    let _framing_length = pdu_length(r.u16()?);
    let framing_vector_offset = r.position();
    let framing_vector = r.u32()?;
    match framing_vector {
        VECTOR_E131_EXTENDED_SYNCHRONIZATION => Ok(Payload::Sync(parse_sync(r)?)),
        VECTOR_E131_EXTENDED_DISCOVERY => Ok(Payload::UniverseDiscovery(parse_discovery(r)?)),
        other => Err(err(
            framing_vector_offset,
            CodecErrorKind::UnknownVector {
                layer: VectorLayer::Framing,
                value: other,
            },
        )),
    }
}

/// Parses a synchronization packet. `r` is positioned after the framing
/// vector.
fn parse_sync(r: &mut Reader<'_>) -> Result<SyncPacket, CodecError> {
    let sequence_number = SequenceNumber::new(r.u8()?);
    let sync_address = r.u16()?;
    // Trailing reserved field is ignored per E1.31, but must be present.
    r.skip(2)?;
    Ok(SyncPacket {
        sequence_number,
        sync_address,
    })
}

/// Parses a universe discovery packet. `r` is positioned after the framing
/// vector.
fn parse_discovery<'a>(r: &mut Reader<'a>) -> Result<UniverseDiscoveryPacket<'a>, CodecError> {
    let source_name = parse_source_name(r.bytes(SOURCE_NAME_LEN)?);
    // Framing-layer reserved field, ignored.
    r.skip(4)?;

    // Universe discovery layer.
    let layer_length_offset = r.position();
    let layer_length = pdu_length(r.u16()?);
    let layer_vector_offset = r.position();
    let layer_vector = r.u32()?;
    if layer_vector != VECTOR_UNIVERSE_DISCOVERY_UNIVERSE_LIST {
        return Err(err(
            layer_vector_offset,
            CodecErrorKind::UnknownVector {
                layer: VectorLayer::UniverseDiscovery,
                value: layer_vector,
            },
        ));
    }
    let page = r.u8()?;
    let last_page = r.u8()?;

    if layer_length < UNIVERSE_DISCOVERY_LAYER_HEADER {
        return Err(err(
            layer_length_offset,
            CodecErrorKind::InvalidLength {
                declared: layer_length,
                available: r.remaining() + (r.position() - layer_length_offset),
            },
        ));
    }
    let list_len = layer_length - UNIVERSE_DISCOVERY_LAYER_HEADER;
    if !list_len.is_multiple_of(2) || list_len > r.remaining() {
        return Err(err(
            layer_length_offset,
            CodecErrorKind::InvalidLength {
                declared: list_len,
                available: r.remaining(),
            },
        ));
    }
    // A page lists at most 512 universes (E1.31 §8); reject an over-length list
    // here so a parsed packet is always re-serializable.
    if list_len / 2 > MAX_UNIVERSES_PER_PAGE {
        return Err(err(
            layer_length_offset,
            CodecErrorKind::TooManyValues {
                count: list_len / 2,
                max: MAX_UNIVERSES_PER_PAGE,
            },
        ));
    }
    let universes = UniverseList::from_bytes(r.bytes(list_len)?);

    Ok(UniverseDiscoveryPacket {
        source_name,
        page,
        last_page,
        universes,
    })
}

// --- Serialization -----------------------------------------------------------

impl Packet<'_> {
    /// The number of bytes [`serialize`](Self::serialize) will write for this
    /// packet.
    pub fn serialized_len(&self) -> usize {
        match &self.payload {
            Payload::Data(d) => DATA_HEADER_SIZE + d.values.len(),
            Payload::Sync(_) => SYNC_PACKET_SIZE,
            Payload::UniverseDiscovery(u) => {
                UNIVERSE_DISCOVERY_HEADER_SIZE + u.universes.as_bytes().len()
            }
        }
    }

    /// Serializes the packet into `out`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns [`CodecErrorKind::BufferTooSmall`] if `out` is shorter than
    /// [`serialized_len`](Self::serialized_len), or
    /// [`CodecErrorKind::TooManyValues`] if the packet carries more DMX slots or
    /// universes than the protocol permits.
    pub fn serialize(&self, out: &mut [u8]) -> Result<usize, CodecError> {
        let mut w = Writer::new(out);
        match &self.payload {
            Payload::Data(d) => serialize_data(&mut w, self.cid, d)?,
            Payload::Sync(s) => serialize_sync(&mut w, self.cid, s)?,
            Payload::UniverseDiscovery(u) => serialize_discovery(&mut w, self.cid, u)?,
        }
        Ok(w.position())
    }

    /// Serializes the packet into a freshly allocated `Vec`.
    #[cfg(feature = "alloc")]
    pub fn to_vec(&self) -> Result<Vec<u8>, CodecError> {
        let mut out = alloc::vec![0u8; self.serialized_len()];
        let n = self.serialize(&mut out)?;
        out.truncate(n);
        Ok(out)
    }
}

/// Writes the ACN root layer: preamble, flags-and-length, vector and CID.
fn serialize_root(
    w: &mut Writer<'_>,
    total_len: usize,
    extended: bool,
    cid: Cid,
) -> Result<(), CodecError> {
    w.bytes(&ACN_PREAMBLE)?;
    w.u16(flags_and_length(total_len - ROOT_FLAGS_LENGTH_OFFSET))?;
    w.u32(if extended {
        VECTOR_ROOT_E131_EXTENDED
    } else {
        VECTOR_ROOT_E131_DATA
    })?;
    w.bytes(cid.as_bytes())?;
    Ok(())
}

/// Writes the fixed 64-byte source-name field: up to 63 bytes of `name`
/// truncated at a UTF-8 character boundary, NUL-padded to the full width.
fn serialize_source_name(w: &mut Writer<'_>, name: &str) -> Result<(), CodecError> {
    let mut n = name.len().min(SOURCE_NAME_LEN - 1);
    while n > 0 && !name.is_char_boundary(n) {
        n -= 1;
    }
    w.bytes(&name.as_bytes()[..n])?;
    w.zeros(SOURCE_NAME_LEN - n)?;
    Ok(())
}

fn serialize_data(w: &mut Writer<'_>, cid: Cid, d: &DataPacket<'_>) -> Result<(), CodecError> {
    let slots = d.values.len();
    if slots > MAX_SLOTS {
        return Err(err(
            0,
            CodecErrorKind::TooManyValues {
                count: slots,
                max: MAX_SLOTS,
            },
        ));
    }
    let total = DATA_HEADER_SIZE + slots;

    serialize_root(w, total, false, cid)?;

    // Framing layer.
    w.u16(flags_and_length(total - FRAMING_OFFSET))?;
    w.u32(VECTOR_E131_DATA_PACKET)?;
    serialize_source_name(w, d.source_name)?;
    w.u8(d.priority)?;
    w.u16(d.sync_address)?;
    w.u8(d.sequence_number.get())?;
    let mut options = 0u8;
    if d.preview {
        options |= OPT_PREVIEW;
    }
    if d.stream_terminated {
        options |= OPT_TERMINATED;
    }
    if d.force_sync {
        options |= OPT_FORCE_SYNC;
    }
    w.u8(options)?;
    w.u16(d.universe)?;

    // DMP layer.
    w.u16(flags_and_length(total - DMP_OFFSET))?;
    w.u8(VECTOR_DMP_SET_PROPERTY)?;
    w.u8(DMP_ADDRESS_DATA_TYPE)?;
    w.u16(DMP_FIRST_PROPERTY_ADDRESS)?;
    w.u16(DMP_ADDRESS_INCREMENT)?;
    w.u16((slots + 1) as u16)?;
    w.u8(d.start_code)?;
    w.bytes(d.values)?;
    Ok(())
}

fn serialize_sync(w: &mut Writer<'_>, cid: Cid, s: &SyncPacket) -> Result<(), CodecError> {
    serialize_root(w, SYNC_PACKET_SIZE, true, cid)?;
    w.u16(flags_and_length(SYNC_PACKET_SIZE - FRAMING_OFFSET))?;
    w.u32(VECTOR_E131_EXTENDED_SYNCHRONIZATION)?;
    w.u8(s.sequence_number.get())?;
    w.u16(s.sync_address)?;
    w.zeros(2)?; // Reserved.
    Ok(())
}

fn serialize_discovery(
    w: &mut Writer<'_>,
    cid: Cid,
    u: &UniverseDiscoveryPacket<'_>,
) -> Result<(), CodecError> {
    let list = u.universes.as_bytes();
    if list.len() / 2 > MAX_UNIVERSES_PER_PAGE {
        return Err(err(
            0,
            CodecErrorKind::TooManyValues {
                count: list.len() / 2,
                max: MAX_UNIVERSES_PER_PAGE,
            },
        ));
    }
    let total = UNIVERSE_DISCOVERY_HEADER_SIZE + list.len();

    serialize_root(w, total, true, cid)?;

    // Framing layer.
    w.u16(flags_and_length(total - FRAMING_OFFSET))?;
    w.u32(VECTOR_E131_EXTENDED_DISCOVERY)?;
    serialize_source_name(w, u.source_name)?;
    w.zeros(4)?; // Reserved.

    // Universe discovery layer.
    w.u16(flags_and_length(total - UNIVERSE_DISCOVERY_OFFSET))?;
    w.u32(VECTOR_UNIVERSE_DISCOVERY_UNIVERSE_LIST)?;
    w.u8(u.page)?;
    w.u8(u.last_page)?;
    w.bytes(list)?;
    Ok(())
}

#[cfg(all(test, feature = "alloc"))]
mod tests;
