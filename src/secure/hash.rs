//! The two BLAKE2s primitives the Pathway Secure sACN extension uses, wrapping
//! the [`blake2`] crate.
//!
//! - The **key fingerprint** is an un-keyed BLAKE2s of the padded key with a
//!   4-byte output. It is a pure function of the password, so a receiver can use
//!   it to pick which candidate key to validate a packet against.
//! - The **message digest** is a keyed BLAKE2s (the padded key is the MAC key)
//!   with a 16-byte output, computed over the UDP payload up to and including the
//!   sequence field.
//!
//! Both fold the output length (and, for the digest, the key length) into the
//! BLAKE2 parameter block per RFC 7693, so the variable-output and keyed-MAC
//! constructions here match `hashlib.blake2s(..., digest_size=N[, key=...])`.

use blake2::Blake2sMac;
use blake2::Blake2sVar;
use blake2::digest::consts::U16;
use blake2::digest::{Mac, Update, VariableOutput};

use super::{DIGEST_LEN, FINGERPRINT_LEN, KEY_LEN};

/// The un-keyed 4-byte BLAKE2s fingerprint of a padded key.
pub(super) fn fingerprint(key: &[u8; KEY_LEN]) -> [u8; FINGERPRINT_LEN] {
    let mut hasher = Blake2sVar::new(FINGERPRINT_LEN).expect("4 is a valid BLAKE2s output size");
    hasher.update(key);
    let mut out = [0u8; FINGERPRINT_LEN];
    hasher
        .finalize_variable(&mut out)
        .expect("output buffer matches the configured size");
    out
}

/// The keyed 16-byte BLAKE2s message digest of `prefix` under `key`.
pub(super) fn digest(prefix: &[u8], key: &[u8; KEY_LEN]) -> [u8; DIGEST_LEN] {
    let mut mac = Blake2sMac::<U16>::new_from_slice(key).expect("a 32-byte key is valid");
    Mac::update(&mut mac, prefix);
    let out = mac.finalize().into_bytes();
    let mut digest = [0u8; DIGEST_LEN];
    digest.copy_from_slice(&out);
    digest
}

/// Whether `tag` is the correct keyed digest of `prefix` under `key`. The
/// comparison is constant-time (via the MAC's `verify_slice`).
pub(super) fn verify(prefix: &[u8], key: &[u8; KEY_LEN], tag: &[u8]) -> bool {
    let mut mac = Blake2sMac::<U16>::new_from_slice(key).expect("a 32-byte key is valid");
    Mac::update(&mut mac, prefix);
    mac.verify_slice(tag).is_ok()
}
