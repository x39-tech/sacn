//! Pathway Secure Streaming ACN: authenticated sACN transmission and receive
//! validation.
//!
//! This is a proof-of-concept implementation of Pathway Connectivity's
//! authenticated extension to ANSI E1.31. It appends a cryptographic message
//! digest (a keyed BLAKE2s MAC) to each data packet so that a receiver sharing
//! the secret password can verify the packet's authenticity and integrity, and
//! reject packets from transmitters that do not hold the password. A per-source
//! sequence guards against replay of previously captured packets.
//!
//! # Wire format
//!
//! A secure packet is an ordinary E1.31 data packet with three changes:
//!
//! - the root layer's post-amble size field is set to
//!   [`POSTAMBLE_SIZE`] (28) instead of zero,
//! - the root layer vector is set to [`SECURE_ROOT_VECTOR`]
//!   (`0x50430001`, a Pathway manufacturer-specific vector) instead of the
//!   standard data vector, and
//! - a 28-byte post-amble is appended after the normal payload:
//!
//! | Size | Field |
//! | ---- | ----- |
//! | 4    | key fingerprint |
//! | 1    | sequence type |
//! | 7    | sequence |
//! | 16   | message digest |
//!
//! The **key fingerprint** is the un-keyed BLAKE2s hash of the padded key, used
//! by the receiver to select which candidate key to validate against. The
//! **message digest** is the keyed BLAKE2s MAC over the entire UDP payload up to
//! and including the sequence field (i.e. every byte except the digest itself).
//!
//! A receiver that does not understand the extension simply ignores the trailing
//! post-amble, so secure packets remain wire-compatible with the base protocol.
//!
//! # Usage
//!
//! On the transmit side, configure a [`Source`](crate::Source) with a
//! [`SecureKey`] via [`SourceConfig::with_pathway_secure`]; its data packets are
//! then signed automatically.
//!
//! On the receive side, build a [`SecureValidator`] from the candidate keys a
//! packet might arrive with and feed each datagram through
//! [`SecureValidator::check`] before acting on it. The tokio
//! [`BasicReceiver`](crate::tokio::BasicReceiver) and
//! [`Receiver`](crate::tokio::Receiver) do this for you when configured with
//! their `with_pathway_secure_keys` builder methods.
//!
//! [`SourceConfig::with_pathway_secure`]: crate::SourceConfig::with_pathway_secure

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::types::Cid;

mod hash;

#[cfg(test)]
mod tests;

/// The Pathway manufacturer-specific root-layer vector that marks a secure
/// packet (`0x50430001`).
const SECURE_ROOT_VECTOR: u32 = crate::packet::VECTOR_ROOT_E131_PATHWAY_SECURE;

/// The size of the secure post-amble, in bytes, as carried in the root layer's
/// post-amble size field.
pub const POSTAMBLE_SIZE: usize = 28;

const KEY_LEN: usize = 32;
const FINGERPRINT_LEN: usize = 4;
const SEQUENCE_LEN: usize = 7;
const DIGEST_LEN: usize = 16;

/// The largest sequence value representable in the 7-byte sequence field.
const SEQUENCE_MAX: u64 = (1 << (8 * SEQUENCE_LEN as u64)) - 1;

const POSTAMBLE_SIZE_OFFSET: usize = 2;
const ROOT_VECTOR_OFFSET: usize = 18;
const CID_OFFSET: usize = 22;

/// The kind of transmitter sequence carried in a secure packet, used to prevent
/// replay attacks.
///
/// Regardless of type, a receiver validates the sequence identically: it must be
/// strictly greater than the previous value accepted from the same source (CID).
/// The type only tells the receiver how the transmitter generates the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceType {
    /// Milliseconds since the Unix epoch.
    Time,
    /// A simple incrementing counter, reset on reboot.
    Volatile,
    /// An incrementing counter guaranteed monotonic across reboots.
    NonVolatile,
}

impl SequenceType {
    /// The on-the-wire sequence-type byte.
    const fn wire(self) -> u8 {
        match self {
            SequenceType::Time => 0,
            SequenceType::Volatile => 1,
            SequenceType::NonVolatile => 2,
        }
    }
}

/// A shared secret key for Pathway Secure sACN: the password, null-padded (or
/// truncated) to 32 bytes, together with its precomputed fingerprint.
///
/// The password may be any byte string; if shorter than 32 bytes it is padded
/// with nulls, and if longer it is truncated to 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SecureKey {
    key: [u8; KEY_LEN],
    fingerprint: [u8; FINGERPRINT_LEN],
}

impl SecureKey {
    /// Derives a key from a password (null-padded or truncated to 32 bytes) and
    /// precomputes its fingerprint.
    pub fn new(password: &[u8]) -> Self {
        let mut key = [0u8; KEY_LEN];
        let n = password.len().min(KEY_LEN);
        key[..n].copy_from_slice(&password[..n]);
        let fingerprint = hash::fingerprint(&key);
        Self { key, fingerprint }
    }

    /// The 4-byte fingerprint of this key: the un-keyed BLAKE2s hash of the
    /// padded key, transmitted in every packet so a receiver can select the key.
    pub fn fingerprint(&self) -> [u8; FINGERPRINT_LEN] {
        self.fingerprint
    }
}

impl core::fmt::Debug for SecureKey {
    /// Prints only the fingerprint, never the key material.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SecureKey")
            .field("fingerprint", &FingerprintHex(self.fingerprint))
            .finish_non_exhaustive()
    }
}

/// Helper to print a fingerprint as hex in [`SecureKey`]'s `Debug`.
struct FingerprintHex([u8; FINGERPRINT_LEN]);

impl core::fmt::Debug for FingerprintHex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Wraps an already-serialized sACN data packet, held in `buf[..len]`, as a
/// Pathway Secure packet in place, returning the new total length.
///
/// It patches the post-amble size field and root-layer vector, then appends the
/// 28-byte secure post-amble (key fingerprint, sequence type, the low 7 bytes of
/// `sequence`, and the keyed message digest). `buf` must have room for at least
/// `len + POSTAMBLE_SIZE` bytes; the packet at `buf[..len]` must be at least
/// `CID_OFFSET` bytes long.
pub(crate) fn secure_in_place(
    buf: &mut [u8],
    len: usize,
    key: &SecureKey,
    seq_type: SequenceType,
    sequence: u64,
) -> usize {
    debug_assert!(
        len >= CID_OFFSET,
        "packet too short to be a valid sACN packet"
    );
    debug_assert!(
        buf.len() >= len + POSTAMBLE_SIZE,
        "buffer must have room for the secure post-amble"
    );

    // Patch the two header fields the extension changes.
    buf[POSTAMBLE_SIZE_OFFSET..POSTAMBLE_SIZE_OFFSET + 2]
        .copy_from_slice(&(POSTAMBLE_SIZE as u16).to_be_bytes());
    buf[ROOT_VECTOR_OFFSET..ROOT_VECTOR_OFFSET + 4]
        .copy_from_slice(&SECURE_ROOT_VECTOR.to_be_bytes());

    // Append the pre-digest post-amble fields.
    let fp_at = len;
    let seq_type_at = fp_at + FINGERPRINT_LEN;
    let seq_at = seq_type_at + 1;
    let digest_at = seq_at + SEQUENCE_LEN;
    buf[fp_at..seq_type_at].copy_from_slice(&key.fingerprint);
    buf[seq_type_at] = seq_type.wire();
    buf[seq_at..digest_at].copy_from_slice(&sequence_bytes(sequence));

    // The digest covers everything up to and including the sequence.
    let digest = hash::digest(&buf[..digest_at], &key.key);
    buf[digest_at..digest_at + DIGEST_LEN].copy_from_slice(&digest);

    digest_at + DIGEST_LEN
}

/// The low 7 bytes of `sequence`, big-endian, as they appear on the wire.
fn sequence_bytes(sequence: u64) -> [u8; SEQUENCE_LEN] {
    let mut out = [0u8; SEQUENCE_LEN];
    let full = (sequence & SEQUENCE_MAX).to_be_bytes();
    out.copy_from_slice(&full[8 - SEQUENCE_LEN..]);
    out
}

/// The security fields located in a raw secure-packet payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SecureFields {
    fingerprint: [u8; FINGERPRINT_LEN],
    sequence: u64,
    cid: Cid,
    /// The index at which the 16-byte digest begins; the digest covers
    /// `payload[..digest_at]`.
    digest_at: usize,
}

/// Whether `payload` carries the Pathway Secure markers (post-amble size 28 and
/// the secure root vector). Cheap structural check; performs no cryptography.
fn is_secure(payload: &[u8]) -> bool {
    payload.len() >= ROOT_VECTOR_OFFSET + 4
        && payload.len() >= POSTAMBLE_SIZE
        && u16::from_be_bytes([
            payload[POSTAMBLE_SIZE_OFFSET],
            payload[POSTAMBLE_SIZE_OFFSET + 1],
        ]) as usize
            == POSTAMBLE_SIZE
        && u32::from_be_bytes([
            payload[ROOT_VECTOR_OFFSET],
            payload[ROOT_VECTOR_OFFSET + 1],
            payload[ROOT_VECTOR_OFFSET + 2],
            payload[ROOT_VECTOR_OFFSET + 3],
        ]) == SECURE_ROOT_VECTOR
}

/// Locates the secure fields in a payload known to be [`is_secure`], returning
/// `None` if it is too short to hold the full post-amble and a CID.
fn secure_fields(payload: &[u8]) -> Option<SecureFields> {
    let len = payload.len();
    if len < POSTAMBLE_SIZE || len < CID_OFFSET + 16 {
        return None;
    }
    let fp_at = len - POSTAMBLE_SIZE;
    let seq_at = fp_at + FINGERPRINT_LEN + 1;
    let digest_at = len - DIGEST_LEN;

    let mut fingerprint = [0u8; FINGERPRINT_LEN];
    fingerprint.copy_from_slice(&payload[fp_at..fp_at + FINGERPRINT_LEN]);

    let mut seq = [0u8; 8];
    seq[8 - SEQUENCE_LEN..].copy_from_slice(&payload[seq_at..seq_at + SEQUENCE_LEN]);
    let sequence = u64::from_be_bytes(seq);

    let mut cid = [0u8; 16];
    cid.copy_from_slice(&payload[CID_OFFSET..CID_OFFSET + 16]);

    Some(SecureFields {
        fingerprint,
        sequence,
        cid: Cid::from_bytes(cid),
        digest_at,
    })
}

/// The result of validating a datagram against a [`SecureValidator`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecureOutcome {
    /// The packet is a valid, authentic, non-replayed secure packet.
    Accepted,
    /// The packet does not carry the secure markers at all (an ordinary,
    /// unauthenticated sACN packet). A secure receiver drops these.
    Unsecured,
    /// The packet is a secure packet but failed validation.
    Rejected(SecureRejection),
}

/// Why a secure packet was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecureRejection {
    /// The packet was too short to contain a well-formed secure post-amble.
    Malformed,
    /// No candidate key matched the packet's fingerprint.
    UnknownKey,
    /// A key matched the fingerprint, but the message digest did not verify:
    /// the packet was forged or modified in transit.
    InvalidDigest,
    /// The sequence was not greater than the last one accepted from this source,
    /// so the packet is a replay (or arrived out of order).
    Replay,
}

/// Validates incoming secure sACN packets against a set of candidate keys and
/// tracks per-source sequences to reject replays.
///
/// A receiver is configured with every key a packet might legitimately arrive
/// with. For each datagram, [`check`](Self::check) uses the packet's fingerprint
/// to pick the matching key, verifies the keyed message digest, and confirms the
/// sequence advances past the last one accepted from that source (CID).
#[derive(Debug)]
pub struct SecureValidator {
    keys: Vec<SecureKey>,
    /// The last accepted sequence per source, keyed by CID.
    last_sequence: BTreeMap<Cid, u64>,
}

impl SecureValidator {
    /// Builds a validator from the candidate keys packets may be signed with.
    pub fn new(keys: impl IntoIterator<Item = SecureKey>) -> Self {
        Self {
            keys: keys.into_iter().collect(),
            last_sequence: BTreeMap::new(),
        }
    }

    /// Whether the validator holds no candidate keys (so it can accept nothing).
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Validates a raw UDP payload.
    ///
    /// Returns [`SecureOutcome::Accepted`] only for a packet that carries the
    /// secure markers, matches one of the candidate keys by fingerprint, whose
    /// keyed digest verifies, and whose sequence advances past the last accepted
    /// one from the same source. A packet without the secure markers yields
    /// [`SecureOutcome::Unsecured`]; any other failure yields a
    /// [`SecureOutcome::Rejected`] with the reason.
    ///
    /// On acceptance the source's stored sequence is advanced, so a subsequent
    /// replay of the same (or an older) packet is rejected.
    pub fn check(&mut self, payload: &[u8]) -> SecureOutcome {
        if !is_secure(payload) {
            return SecureOutcome::Unsecured;
        }
        let Some(fields) = secure_fields(payload) else {
            return SecureOutcome::Rejected(SecureRejection::Malformed);
        };

        let Some(key) = self
            .keys
            .iter()
            .find(|k| k.fingerprint == fields.fingerprint)
        else {
            return SecureOutcome::Rejected(SecureRejection::UnknownKey);
        };

        let (prefix, digest) = payload.split_at(fields.digest_at);
        if !hash::verify(prefix, &key.key, digest) {
            return SecureOutcome::Rejected(SecureRejection::InvalidDigest);
        }

        // Replay protection: the sequence must strictly advance per source.
        if let Some(&previous) = self.last_sequence.get(&fields.cid)
            && fields.sequence <= previous
        {
            return SecureOutcome::Rejected(SecureRejection::Replay);
        }
        self.last_sequence.insert(fields.cid, fields.sequence);

        SecureOutcome::Accepted
    }

    /// Forgets the tracked sequence for a source, so its next packet is accepted
    /// regardless of sequence. Used when a source is considered lost, matching
    /// the extension's guidance to reset per-source sequence state on stream
    /// termination or timeout.
    pub fn forget_source(&mut self, cid: &Cid) {
        self.last_sequence.remove(cid);
    }
}
