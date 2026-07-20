//! Unit tests for the Pathway Secure sACN primitives.
//!
//! The fingerprint and digest expectations are cross-checked against the
//! reference Python implementation (`hashlib.blake2s`), so a mismatch means our
//! BLAKE2s parameterization has drifted from the spec.

use super::*;

/// `blake2s(pad(password), digest_size=4)`, from the reference implementation.
#[test]
fn key_fingerprints_match_reference() {
    assert_eq!(
        SecureKey::new(b"showtime").fingerprint(),
        [0xa6, 0x3e, 0x07, 0x30]
    );
    assert_eq!(SecureKey::new(b"").fingerprint(), [0xf9, 0xab, 0x06, 0x54]);
    assert_eq!(
        SecureKey::new(&[b'a'; 40]).fingerprint(),
        [0x60, 0x3a, 0x1a, 0xf1]
    );
    assert_eq!(
        SecureKey::new(b"hunter2").fingerprint(),
        [0xc7, 0xa9, 0xf1, 0xdd]
    );
}

/// An over-length password is truncated to 32 bytes: 40 'a's and 32 'a's share
/// a padded key, hence a fingerprint.
#[test]
fn overlength_password_truncates_to_32_bytes() {
    assert_eq!(
        SecureKey::new(&[b'a'; 40]).fingerprint(),
        SecureKey::new(&[b'a'; 32]).fingerprint()
    );
}

/// `blake2s(bytes(range(50)), key=pad(password), digest_size=16)`, from the
/// reference implementation.
#[test]
fn message_digests_match_reference() {
    let prefix: [u8; 50] = core::array::from_fn(|i| i as u8);
    assert_eq!(
        hash::digest(&prefix, &SecureKey::new(b"showtime").key),
        [
            0x35, 0x6e, 0x42, 0x02, 0xbc, 0xc1, 0xa6, 0x54, 0xc4, 0xef, 0xf7, 0xec, 0x44, 0x02,
            0x5e, 0xe6,
        ]
    );
    assert_eq!(
        hash::digest(&prefix, &SecureKey::new(b"hunter2").key),
        [
            0x4a, 0x97, 0xec, 0x34, 0xbd, 0x0b, 0x8d, 0x55, 0x42, 0xd5, 0xba, 0x25, 0x50, 0xc9,
            0x9b, 0xf0,
        ]
    );
}

/// A payload signed by `secure_in_place` is recognized and accepted by a
/// validator holding the same key.
#[test]
fn round_trip_secure_and_validate() {
    let key = SecureKey::new(b"showtime");
    // A plausible 126-byte data-packet header stand-in with a nonzero CID.
    let mut buf = [0u8; 256];
    let base_len = 126;
    buf[CID_OFFSET..CID_OFFSET + 16].copy_from_slice(&[0x11; 16]);
    let len = secure_in_place(&mut buf, base_len, &key, SequenceType::Volatile, 1);

    assert_eq!(len, base_len + POSTAMBLE_SIZE);
    assert!(is_secure(&buf[..len]));

    let mut validator = SecureValidator::new([key]);
    assert_eq!(validator.check(&buf[..len]), SecureOutcome::Accepted);
}

/// A validator without the signing key rejects an otherwise-valid packet as
/// having an unknown key (the fingerprints do not match).
#[test]
fn wrong_key_is_rejected() {
    let signing = SecureKey::new(b"showtime");
    let mut buf = [0u8; 256];
    buf[CID_OFFSET..CID_OFFSET + 16].copy_from_slice(&[0x22; 16]);
    let len = secure_in_place(&mut buf, 126, &signing, SequenceType::Volatile, 1);

    let mut validator = SecureValidator::new([SecureKey::new(b"different")]);
    assert_eq!(
        validator.check(&buf[..len]),
        SecureOutcome::Rejected(SecureRejection::UnknownKey)
    );
}

/// A payload whose fingerprint matches a held key but whose body was tampered
/// with fails digest verification.
#[test]
fn tampered_payload_fails_digest() {
    let key = SecureKey::new(b"showtime");
    let mut buf = [0u8; 256];
    buf[CID_OFFSET..CID_OFFSET + 16].copy_from_slice(&[0x33; 16]);
    let len = secure_in_place(&mut buf, 126, &key, SequenceType::Volatile, 5);

    // Flip a byte inside the signed region (not the digest).
    buf[60] ^= 0xff;

    let mut validator = SecureValidator::new([key]);
    assert_eq!(
        validator.check(&buf[..len]),
        SecureOutcome::Rejected(SecureRejection::InvalidDigest)
    );
}

/// Re-presenting an accepted packet (same sequence) is a replay; a strictly
/// greater sequence is accepted.
#[test]
fn replayed_sequence_is_rejected() {
    let key = SecureKey::new(b"showtime");
    let cid = [0x44; 16];

    let sign = |sequence: u64, out: &mut [u8]| {
        out[CID_OFFSET..CID_OFFSET + 16].copy_from_slice(&cid);
        secure_in_place(out, 126, &key, SequenceType::Volatile, sequence)
    };

    let mut validator = SecureValidator::new([key]);

    let mut first = [0u8; 256];
    let n1 = sign(1, &mut first);
    assert_eq!(validator.check(&first[..n1]), SecureOutcome::Accepted);

    // Replaying sequence 1 is rejected.
    assert_eq!(
        validator.check(&first[..n1]),
        SecureOutcome::Rejected(SecureRejection::Replay)
    );

    // A greater sequence from the same source advances and is accepted.
    let mut second = [0u8; 256];
    let n2 = sign(2, &mut second);
    assert_eq!(validator.check(&second[..n2]), SecureOutcome::Accepted);

    // After forgetting the source, an old sequence is accepted again.
    validator.forget_source(&Cid::from_bytes(cid));
    assert_eq!(validator.check(&first[..n1]), SecureOutcome::Accepted);
}

/// A packet with no secure markers is reported as unsecured, not rejected.
#[test]
fn unsecured_packet_is_detected() {
    let mut validator = SecureValidator::new([SecureKey::new(b"showtime")]);
    let plain = [0u8; 126];
    assert_eq!(validator.check(&plain), SecureOutcome::Unsecured);
}

/// End-to-end: a data packet emitted by a secure [`Source`] carries the secure
/// markers, parses as an ordinary data packet, validates against the signing
/// key, and is rejected by a validator holding a different key.
#[test]
fn source_emits_validatable_secure_packets() {
    use crate::packet::{Packet, Payload};
    use crate::time::Instant;
    use crate::{Cid, Route, Source, SourceConfig, Universe, UniverseConfig};

    let key = SecureKey::new(b"showtime");
    let mut source = Source::new(
        SourceConfig::new(Cid::from_bytes([9; 16]), "secure source").with_pathway_secure(key),
    );
    let universe = Universe::new(1).unwrap();
    source.add_universe(UniverseConfig::new(universe)).unwrap();
    source.update_levels(universe, &[10, 20, 30]);

    let mut poll = source.poll(Instant::EPOCH);
    while let Some(tx) = poll.next_transmission() {
        if !matches!(tx.route, Route::Universe(_)) {
            continue;
        }
        // The bytes carry the Pathway Secure markers.
        assert!(is_secure(tx.data));

        // A legacy parser still reads the inner data packet (ignoring the
        // post-amble).
        let packet = Packet::parse(tx.data).expect("secure packet parses as data");
        match packet.payload {
            Payload::Data(data) => {
                assert_eq!(data.universe, 1);
                assert_eq!(data.values, &[10, 20, 30]);
            }
            other => panic!("expected a data packet, got {other:?}"),
        }

        // The signing key validates it; a different key does not.
        assert_eq!(
            SecureValidator::new([key]).check(tx.data),
            SecureOutcome::Accepted
        );
        assert_eq!(
            SecureValidator::new([SecureKey::new(b"other")]).check(tx.data),
            SecureOutcome::Rejected(SecureRejection::UnknownKey)
        );
        return;
    }
    panic!("source emitted no universe data transmission");
}
