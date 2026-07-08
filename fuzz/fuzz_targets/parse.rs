//! Fuzz target for the sACN packet parser.
//!
//! Two invariants are checked on every input:
//!
//! 1. Parsing arbitrary bytes never panics - it either yields a [`Packet`] or a
//!    structured error.
//! 2. Any packet that parses must re-serialize and re-parse to an identical
//!    value (parse/serialize round-trip symmetry on valid inputs).

#![no_main]

use libfuzzer_sys::fuzz_target;
use sacn::packet::{Packet, MAX_PACKET_SIZE};

fuzz_target!(|data: &[u8]| {
    if let Ok(packet) = Packet::parse(data) {
        let mut buf = [0u8; MAX_PACKET_SIZE];
        let n = packet
            .serialize(&mut buf)
            .expect("a parsed packet must re-serialize");
        let reparsed = Packet::parse(&buf[..n]).expect("re-serialized packet must re-parse");
        assert_eq!(packet, reparsed, "round-trip changed the packet");
    }
});
