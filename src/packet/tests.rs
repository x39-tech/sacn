//! Unit, round-trip and randomized tests for the codec.

use super::*;
use crate::error::{CodecError, CodecErrorKind, VectorLayer};

const TEST_CID: Cid = Cid::from_bytes([
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
]);

fn data_packet<'a>(values: &'a [u8], source_name: &'a str) -> Packet<'a> {
    Packet {
        cid: TEST_CID,
        payload: Payload::Data(DataPacket {
            source_name,
            priority: 150,
            sync_address: 7,
            sequence_number: SequenceNumber::new(42),
            preview: true,
            stream_terminated: false,
            force_sync: true,
            universe: 1234,
            start_code: 0x00,
            values,
        }),
    }
}

/// Serialize into a generous buffer and return the exact bytes written.
fn roundtrip_bytes(packet: &Packet<'_>) -> [u8; MAX_PACKET_SIZE] {
    let mut buf = [0u8; MAX_PACKET_SIZE];
    let n = packet.serialize(&mut buf).expect("serialize");
    assert_eq!(n, packet.serialized_len(), "serialized_len matches written");
    buf
}

#[test]
fn data_packet_roundtrips() {
    let values: [u8; 4] = [10, 20, 30, 40];
    let packet = data_packet(&values, "Test Source");
    let buf = roundtrip_bytes(&packet);
    let parsed = Packet::parse(&buf[..packet.serialized_len()]).expect("parse");
    assert_eq!(parsed, packet);
}

#[test]
fn empty_data_packet_roundtrips() {
    let packet = data_packet(&[], "");
    let buf = roundtrip_bytes(&packet);
    assert_eq!(packet.serialized_len(), DATA_HEADER_SIZE);
    let parsed = Packet::parse(&buf[..packet.serialized_len()]).expect("parse");
    assert_eq!(parsed, packet);
}

#[test]
fn full_data_packet_roundtrips() {
    let values = [0xABu8; MAX_SLOTS];
    let packet = data_packet(&values, "Full");
    let buf = roundtrip_bytes(&packet);
    assert_eq!(packet.serialized_len(), DATA_HEADER_SIZE + MAX_SLOTS);
    let parsed = Packet::parse(&buf[..packet.serialized_len()]).expect("parse");
    assert_eq!(parsed, packet);
    if let Payload::Data(d) = parsed.payload {
        assert_eq!(d.values.len(), MAX_SLOTS);
    } else {
        panic!("expected data");
    }
}

#[test]
fn alternate_start_code_preserved() {
    let values = [200u8; 3];
    let mut packet = data_packet(&values, "PAP");
    if let Payload::Data(ref mut d) = packet.payload {
        d.start_code = 0xDD;
    } else {
        panic!("expected data");
    }
    let buf = roundtrip_bytes(&packet);
    let parsed = Packet::parse(&buf[..packet.serialized_len()]).expect("parse");
    assert_eq!(parsed, packet);
}

#[test]
fn sync_packet_roundtrips() {
    let packet = Packet {
        cid: TEST_CID,
        payload: Payload::Sync(SyncPacket {
            sequence_number: SequenceNumber::new(99),
            sync_address: 4242,
        }),
    };
    let buf = roundtrip_bytes(&packet);
    assert_eq!(packet.serialized_len(), SYNC_PACKET_SIZE);
    let parsed = Packet::parse(&buf[..packet.serialized_len()]).expect("parse");
    assert_eq!(parsed, packet);
}

#[test]
fn universe_discovery_roundtrips() {
    // Big-endian universe list 1, 2, 63999.
    let list = [0x00, 0x01, 0x00, 0x02, 0xf9, 0xff];
    let packet = Packet {
        cid: TEST_CID,
        payload: Payload::UniverseDiscovery(UniverseDiscoveryPacket {
            source_name: "Discovery Source",
            page: 0,
            last_page: 0,
            universes: UniverseList::from_bytes(&list),
        }),
    };
    let buf = roundtrip_bytes(&packet);
    let parsed = Packet::parse(&buf[..packet.serialized_len()]).expect("parse");
    assert_eq!(parsed, packet);
    if let Payload::UniverseDiscovery(d) = parsed.payload {
        let universes: alloc::vec::Vec<u16> = d.universes.iter().collect();
        assert_eq!(universes, alloc::vec![1, 2, 63999]);
        assert_eq!(d.universes.len(), 3);
    } else {
        panic!("expected discovery");
    }
}

#[test]
fn empty_universe_discovery_roundtrips() {
    let packet = Packet {
        cid: TEST_CID,
        payload: Payload::UniverseDiscovery(UniverseDiscoveryPacket {
            source_name: "Empty",
            page: 3,
            last_page: 3,
            universes: UniverseList::from_bytes(&[]),
        }),
    };
    let buf = roundtrip_bytes(&packet);
    assert_eq!(packet.serialized_len(), UNIVERSE_DISCOVERY_HEADER_SIZE);
    let parsed = Packet::parse(&buf[..packet.serialized_len()]).expect("parse");
    assert_eq!(parsed, packet);
    if let Payload::UniverseDiscovery(d) = parsed.payload {
        assert!(d.universes.is_empty());
        assert_eq!(d.universes.len(), 0);
    } else {
        panic!("expected discovery");
    }
}

#[test]
fn to_vec_matches_serialize() {
    // `to_vec` is the alloc convenience wrapper around `serialize`; it should
    // produce exactly the same bytes.
    let packet = data_packet(&[10, 20, 30], "vec");
    let vec = packet.to_vec().expect("to_vec");
    assert_eq!(vec.len(), packet.serialized_len());
    let mut buf = [0u8; MAX_PACKET_SIZE];
    let n = packet.serialize(&mut buf).unwrap();
    assert_eq!(vec.as_slice(), &buf[..n]);
    assert_eq!(Packet::parse(&vec).unwrap(), packet);
}

#[test]
fn universe_list_into_iter_matches_iter() {
    // Exercise the by-value `IntoIterator` path, which is distinct from `iter`.
    let list = [0x00, 0x01, 0x00, 0x02, 0xf9, 0xff];
    let universes = UniverseList::from_bytes(&list);
    let by_ref: alloc::vec::Vec<u16> = universes.iter().collect();
    let by_value: alloc::vec::Vec<u16> = universes.into_iter().collect();
    assert_eq!(by_ref, alloc::vec![1, 2, 63999]);
    assert_eq!(by_ref, by_value);
}

#[test]
fn options_byte_maps_each_flag_to_its_documented_bit() {
    // Options field is at offset 112, with preview = 0x80,
    // stream_terminated = 0x40, force_sync = 0x20 (E1.31 §6.2.6).
    const OPTS_OFFSET: usize = 112;
    for bits in 0u8..8 {
        let preview = bits & 0b100 != 0;
        let terminated = bits & 0b010 != 0;
        let force_sync = bits & 0b001 != 0;

        let mut packet = data_packet(&[1, 2, 3], "Flags");
        if let Payload::Data(ref mut d) = packet.payload {
            d.preview = preview;
            d.stream_terminated = terminated;
            d.force_sync = force_sync;
        } else {
            panic!("expected data");
        }

        let buf = roundtrip_bytes(&packet);
        let expected =
            (u8::from(preview) << 7) | (u8::from(terminated) << 6) | (u8::from(force_sync) << 5);
        assert_eq!(
            buf[OPTS_OFFSET], expected,
            "options byte for bits {bits:#05b}"
        );

        let parsed = Packet::parse(&buf[..packet.serialized_len()]).expect("parse");
        assert_eq!(parsed, packet);
    }
}

#[test]
fn secure_marked_data_packet_parses_as_plain() {
    let values: [u8; 4] = [10, 20, 30, 40];
    let plain = data_packet(&values, "Secure Source");

    let mut buf = [0u8; MAX_PACKET_SIZE];
    let n = plain.serialize(&mut buf).expect("serialize");

    // Apply the Pathway Secure header markers and append a dummy 28-byte
    // post-amble (contents are ignored by the base parser).
    buf[2..4].copy_from_slice(&28u16.to_be_bytes());
    buf[18..22].copy_from_slice(&VECTOR_ROOT_E131_PATHWAY_SECURE.to_be_bytes());
    let secure_len = n + 28;

    let parsed = Packet::parse(&buf[..secure_len]).expect("secure-marked packet parses");
    assert_eq!(
        parsed, plain,
        "secure markers and post-amble are transparent"
    );

    // And it re-serializes to the plain form, which re-parses identically.
    let reserialized = roundtrip_bytes(&parsed);
    let reparsed = Packet::parse(&reserialized[..parsed.serialized_len()])
        .expect("re-serialized packet parses");
    assert_eq!(reparsed, plain);
}

// --- Parse error cases -------------------------------------------------------

#[test]
fn rejects_short_buffer() {
    // Two bytes in: the 16-byte preamble read fails needing 14 more, at offset 0.
    assert_eq!(
        Packet::parse(&[0x00, 0x10]).unwrap_err(),
        CodecError {
            offset: 0,
            kind: CodecErrorKind::UnexpectedEof { needed: 14 },
        }
    );
}

#[test]
fn rejects_bad_preamble() {
    let mut buf = [0u8; DATA_HEADER_SIZE];
    let packet = data_packet(&[], "x");
    packet.serialize(&mut buf).unwrap();
    buf[0] = 0xFF; // Corrupt the preamble.
    assert_eq!(
        Packet::parse(&buf).unwrap_err(),
        CodecError {
            offset: 0,
            kind: CodecErrorKind::InvalidPreamble,
        }
    );
}

#[test]
fn rejects_unknown_root_vector() {
    let mut buf = [0u8; DATA_HEADER_SIZE];
    let packet = data_packet(&[], "x");
    packet.serialize(&mut buf).unwrap();
    buf[18..22].copy_from_slice(&0x0000_0099u32.to_be_bytes());
    assert_eq!(
        Packet::parse(&buf).unwrap_err(),
        CodecError {
            offset: 18,
            kind: CodecErrorKind::UnknownVector {
                layer: VectorLayer::Root,
                value: 0x99,
            },
        }
    );
}

#[test]
fn rejects_unknown_framing_vector() {
    let mut buf = [0u8; DATA_HEADER_SIZE];
    let packet = data_packet(&[], "x");
    packet.serialize(&mut buf).unwrap();
    buf[40..44].copy_from_slice(&0x0000_0042u32.to_be_bytes());
    assert_eq!(
        Packet::parse(&buf).unwrap_err(),
        CodecError {
            offset: 40,
            kind: CodecErrorKind::UnknownVector {
                layer: VectorLayer::Framing,
                value: 0x42,
            },
        }
    );
}

#[test]
fn out_of_range_priority_passes_through() {
    // The standard gives receivers no rule to discard an out-of-range priority
    // so the codec decodes it faithfully and leaves any clamping to the receiver.
    let mut buf = [0u8; DATA_HEADER_SIZE];
    let packet = data_packet(&[], "x");
    packet.serialize(&mut buf).unwrap();
    buf[108] = 255; // Priority offset.
    let parsed = Packet::parse(&buf).expect("parse");
    if let Payload::Data(d) = parsed.payload {
        assert_eq!(d.priority, 255);
    } else {
        panic!("expected data");
    }
}

#[test]
fn rejects_malformed_dmp_layer() {
    let mut buf = [0u8; DATA_HEADER_SIZE];
    let packet = data_packet(&[], "x");
    packet.serialize(&mut buf).unwrap();
    buf[118] = 0x00; // Address/data type should be 0xa1.
    assert_eq!(
        Packet::parse(&buf).unwrap_err(),
        CodecError {
            offset: 118,
            kind: CodecErrorKind::MalformedDmpLayer,
        }
    );
}

#[test]
fn invalid_utf8_source_name_degrades_to_empty() {
    // An invalid-UTF-8 name is non-conformant but never affects data; the codec
    // yields an empty name rather than dropping the whole packet.
    let packet = data_packet(&[1, 2, 3], "x");
    let mut full = [0u8; DATA_HEADER_SIZE + 3];
    packet.serialize(&mut full).unwrap();
    full[44] = 0xFF; // Invalid UTF-8 lead byte at the start of the name.
    let parsed = Packet::parse(&full).expect("parse");
    if let Payload::Data(d) = parsed.payload {
        assert_eq!(d.source_name, "");
        assert_eq!(d.values, &[1, 2, 3]); // Data still decoded.
    } else {
        panic!("expected data");
    }
}

#[test]
fn rejects_zero_property_value_count() {
    let mut buf = [0u8; DATA_HEADER_SIZE];
    let packet = data_packet(&[], "x");
    packet.serialize(&mut buf).unwrap();
    buf[123..125].copy_from_slice(&0u16.to_be_bytes()); // property value count = 0
    assert_eq!(
        Packet::parse(&buf).unwrap_err(),
        CodecError {
            offset: 123,
            kind: CodecErrorKind::EmptyDmpLayer,
        }
    );
}

#[test]
fn rejects_over_length_slot_count() {
    let values = [7u8; MAX_SLOTS];
    let packet = data_packet(&values, "x");
    // One byte of slack so 513 slots' worth of data is present in the buffer.
    let mut buf = [0u8; DATA_HEADER_SIZE + MAX_SLOTS + 1];
    packet
        .serialize(&mut buf[..DATA_HEADER_SIZE + MAX_SLOTS])
        .unwrap();
    // Bump the Property Value Count (offset 123) from 513 to 514 -> 513 slots.
    buf[123..125].copy_from_slice(&((MAX_SLOTS as u16) + 2).to_be_bytes());
    assert_eq!(
        Packet::parse(&buf).unwrap_err(),
        CodecError {
            offset: 123,
            kind: CodecErrorKind::TooManyValues {
                count: 513,
                max: 512,
            },
        }
    );
}

#[test]
fn rejects_slot_count_overrunning_buffer() {
    let values = [1u8, 2, 3];
    let packet = data_packet(&values, "x");
    let mut buf = [0u8; DATA_HEADER_SIZE + 3];
    packet.serialize(&mut buf).unwrap();
    // Claim more property values than the buffer holds, but within the 512-slot
    // cap so the buffer-overrun check (not the cap check) is what fires.
    buf[123..125].copy_from_slice(&100u16.to_be_bytes());
    // 99 declared slots, but the values start at offset 126 with only 3 bytes
    // left, so the read needs 96 more.
    assert_eq!(
        Packet::parse(&buf).unwrap_err(),
        CodecError {
            offset: 126,
            kind: CodecErrorKind::UnexpectedEof { needed: 96 },
        }
    );
}

#[test]
fn rejects_over_length_universe_list() {
    let list = [0u8; MAX_UNIVERSES_PER_PAGE * 2];
    let packet = Packet {
        cid: TEST_CID,
        payload: Payload::UniverseDiscovery(UniverseDiscoveryPacket {
            source_name: "x",
            page: 0,
            last_page: 0,
            universes: UniverseList::from_bytes(&list),
        }),
    };
    let full = UNIVERSE_DISCOVERY_HEADER_SIZE + MAX_UNIVERSES_PER_PAGE * 2;
    let mut buf = [0u8; UNIVERSE_DISCOVERY_HEADER_SIZE + (MAX_UNIVERSES_PER_PAGE + 1) * 2];
    packet.serialize(&mut buf[..full]).unwrap();
    // Bump the discovery-layer length (offset 112) by 2 so it claims 513
    // universes; the two extra bytes are already present in the buffer.
    let layer_len = full - UNIVERSE_DISCOVERY_OFFSET + 2;
    buf[112..114].copy_from_slice(&(0x7000u16 | layer_len as u16).to_be_bytes());
    assert_eq!(
        Packet::parse(&buf).unwrap_err(),
        CodecError {
            offset: 112,
            kind: CodecErrorKind::TooManyValues {
                count: 513,
                max: 512,
            },
        }
    );
}

// --- Serialize error cases ---------------------------------------------------

#[test]
fn serialize_rejects_small_buffer() {
    let packet = data_packet(&[1, 2, 3], "x");
    let mut buf = [0u8; 10];
    // The 16-byte preamble is the first write; 10 available, 6 short, at offset 0.
    assert_eq!(
        packet.serialize(&mut buf).unwrap_err(),
        CodecError {
            offset: 0,
            kind: CodecErrorKind::BufferTooSmall {
                needed: 6,
                available: 10,
            },
        }
    );
}

#[test]
fn serialize_rejects_too_many_slots() {
    let values = [0u8; MAX_SLOTS + 1];
    let packet = data_packet(&values, "x");
    let mut buf = [0u8; MAX_PACKET_SIZE];
    assert_eq!(
        packet.serialize(&mut buf).unwrap_err(),
        CodecError {
            offset: 0,
            kind: CodecErrorKind::TooManyValues {
                count: 513,
                max: 512,
            },
        }
    );
}

// --- Helper unit tests: source name -----------------------------------------

#[test]
fn parse_source_name_stops_at_nul() {
    let mut field = [0u8; SOURCE_NAME_LEN];
    field[..5].copy_from_slice(b"hello");
    field[10] = b'X'; // Bytes after the NUL must be ignored.
    assert_eq!(parse_source_name(&field), "hello");
}

#[test]
fn parse_source_name_caps_at_63_bytes() {
    // No NUL anywhere: the 64th octet is reserved for the terminator, so the
    // name is the first 63 bytes.
    let field = [b'A'; SOURCE_NAME_LEN];
    assert_eq!(parse_source_name(&field).len(), 63);
}

#[test]
fn parse_source_name_invalid_utf8_is_empty() {
    let mut field = [0u8; SOURCE_NAME_LEN];
    field[0] = 0xFF; // Invalid UTF-8 lead byte.
    field[1] = b'a';
    assert_eq!(parse_source_name(&field), "");
}

#[test]
fn parse_source_name_decodes_multibyte_utf8() {
    let mut field = [0u8; SOURCE_NAME_LEN];
    let s = "caf\u{00e9}";
    field[..s.len()].copy_from_slice(s.as_bytes());
    assert_eq!(parse_source_name(&field), s);
}

#[test]
fn parse_source_name_empty_field_is_empty() {
    assert_eq!(parse_source_name(&[0u8; SOURCE_NAME_LEN]), "");
}

#[test]
fn serialize_source_name_pads_with_nul() {
    let mut buf = [0xffu8; SOURCE_NAME_LEN];
    let mut w = Writer::new(&mut buf);
    serialize_source_name(&mut w, "hi").unwrap();
    assert_eq!(w.position(), SOURCE_NAME_LEN);
    assert_eq!(&buf[..2], b"hi");
    assert!(buf[2..].iter().all(|&b| b == 0));
}

#[test]
fn serialize_source_name_truncates_at_char_boundary() {
    // 32 two-byte chars = 64 bytes. The cap is 63, which falls mid-character, so
    // serialization must back off to 62 bytes (31 chars) rather than split one.
    let name = "\u{00e9}".repeat(32);
    let mut buf = [0u8; SOURCE_NAME_LEN];
    let mut w = Writer::new(&mut buf);
    serialize_source_name(&mut w, &name).unwrap();
    assert_eq!(&buf[..62], &name.as_bytes()[..62]);
    assert_eq!(buf[62], 0); // NUL terminator, not a split character.
    assert_eq!(buf[63], 0);
}

#[test]
fn source_name_round_trips_through_field() {
    for name in ["", "x", "Source 1", "caf\u{00e9}"] {
        let mut buf = [0u8; SOURCE_NAME_LEN];
        let mut w = Writer::new(&mut buf);
        serialize_source_name(&mut w, name).unwrap();
        assert_eq!(parse_source_name(&buf), name);
    }
}

// --- Helper unit tests: universe list ---------------------------------------

#[test]
fn universe_list_iterates_values() {
    let bytes = [0x00, 0x01, 0x00, 0x02, 0xf9, 0xff];
    let list = UniverseList::from_bytes(&bytes);
    assert_eq!(list.len(), 3);
    assert!(!list.is_empty());
    assert_eq!(list.as_bytes(), &bytes);
    assert_eq!(
        list.iter().collect::<alloc::vec::Vec<_>>(),
        alloc::vec![1, 2, 63999]
    );
    assert_eq!(
        list.into_iter().collect::<alloc::vec::Vec<_>>(),
        alloc::vec![1, 2, 63999]
    );
}

#[test]
fn universe_list_empty() {
    let list = UniverseList::from_bytes(&[]);
    assert!(list.is_empty());
    assert_eq!(list.len(), 0);
    assert_eq!(list.iter().count(), 0);
}

#[test]
fn universe_list_ignores_trailing_odd_byte() {
    let bytes = [0x00, 0x01, 0x00, 0x02, 0x99]; // Five bytes: two universes + stray.
    let list = UniverseList::from_bytes(&bytes);
    assert_eq!(list.len(), 2);
    assert_eq!(
        list.iter().collect::<alloc::vec::Vec<_>>(),
        alloc::vec![1, 2]
    );
}

#[test]
fn universe_list_single_byte_is_empty() {
    let list = UniverseList::from_bytes(&[0x07]);
    assert!(list.is_empty());
    assert_eq!(list.len(), 0);
}

#[test]
fn universe_list_debug_lists_universes() {
    let list = UniverseList::from_bytes(&[0x00, 0x01, 0x00, 0x02]);
    assert_eq!(alloc::format!("{list:?}"), "[1, 2]");
}

// --- Helper unit tests: flags and length ------------------------------------

#[test]
fn flags_and_length_sets_flags_and_masks_to_12_bits() {
    assert_eq!(flags_and_length(0), 0x7000);
    assert_eq!(flags_and_length(0x123), 0x7123);
    assert_eq!(flags_and_length(0x0fff), 0x7fff);
    assert_eq!(flags_and_length(0x1000), 0x7000); // Overflow masked off.
}

#[test]
fn pdu_length_extracts_low_12_bits() {
    assert_eq!(pdu_length(0x7123), 0x123);
    assert_eq!(pdu_length(0x0123), 0x123); // Flag nibble ignored.
    assert_eq!(pdu_length(0xffff), 0x0fff);
}

#[test]
fn flags_and_length_round_trips() {
    for len in [0usize, 1, 126, 638, 1144, 0x0fff] {
        assert_eq!(pdu_length(flags_and_length(len)), len);
    }
}

// --- Property tests (proptest) ----------------------------------------------

// These generate valid, representable packets and check the round-trip
// `parse(serialize(packet)) == packet`, shrinking any failure to a minimal
// case. proptest is std-only, so they are gated on `std`; the no_std/alloc-only
// configuration still runs the deterministic unit tests above, and the fuzz
// target covers the adversarial bytes-in direction with coverage guidance.
#[cfg(feature = "std")]
mod property {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::FileFailurePersistence;

    proptest! {
        #![proptest_config(
            ProptestConfig {
                failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel("tests/proptest-regressions"))),
                cases: 512,
                ..Default::default()
            }
        )]

        // Source names are restricted to printable ASCII up to 63 bytes so they
        // survive the round-trip unchanged; longer or NUL-bearing names are
        // normalized by serialization, which the source-name unit tests cover.
        #[test]
        fn data_packet_round_trips(
            cid in any::<[u8; 16]>(),
            source_name in "[ -~]{0,63}",
            priority in any::<u8>(),
            sync_address in any::<u16>(),
            sequence in any::<u8>(),
            preview in any::<bool>(),
            stream_terminated in any::<bool>(),
            force_sync in any::<bool>(),
            universe in any::<u16>(),
            start_code in any::<u8>(),
            values in prop::collection::vec(any::<u8>(), 0..=MAX_SLOTS),
        ) {
            let packet = Packet {
                cid: Cid::from_bytes(cid),
                payload: Payload::Data(DataPacket {
                    source_name: &source_name,
                    priority,
                    sync_address,
                    sequence_number: SequenceNumber::new(sequence),
                    preview,
                    stream_terminated,
                    force_sync,
                    universe,
                    start_code,
                    values: &values,
                }),
            };
            let mut buf = [0u8; MAX_PACKET_SIZE];
            let n = packet.serialize(&mut buf).expect("serialize");
            prop_assert_eq!(Packet::parse(&buf[..n]).expect("parse"), packet);
        }

        #[test]
        fn discovery_packet_round_trips(
            cid in any::<[u8; 16]>(),
            source_name in "[ -~]{0,63}",
            page in any::<u8>(),
            last_page in any::<u8>(),
            universes in prop::collection::vec(any::<u16>(), 0..=MAX_UNIVERSES_PER_PAGE),
        ) {
            let bytes: alloc::vec::Vec<u8> =
                universes.iter().flat_map(|u| u.to_be_bytes()).collect();
            let packet = Packet {
                cid: Cid::from_bytes(cid),
                payload: Payload::UniverseDiscovery(UniverseDiscoveryPacket {
                    source_name: &source_name,
                    page,
                    last_page,
                    universes: UniverseList::from_bytes(&bytes),
                }),
            };
            let mut buf = [0u8; MAX_PACKET_SIZE];
            let n = packet.serialize(&mut buf).expect("serialize");
            prop_assert_eq!(Packet::parse(&buf[..n]).expect("parse"), packet);
        }

        #[test]
        fn sync_packet_round_trips(
            cid in any::<[u8; 16]>(),
            sequence in any::<u8>(),
            sync_address in any::<u16>(),
        ) {
            let packet = Packet {
                cid: Cid::from_bytes(cid),
                payload: Payload::Sync(SyncPacket {
                    sequence_number: SequenceNumber::new(sequence),
                    sync_address,
                }),
            };
            let mut buf = [0u8; MAX_PACKET_SIZE];
            let n = packet.serialize(&mut buf).expect("serialize");
            prop_assert_eq!(Packet::parse(&buf[..n]).expect("parse"), packet);
        }

        // Parsing arbitrary bytes must never panic - it either parses or returns
        // a structured error.
        #[test]
        fn parse_never_panics(data in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = Packet::parse(&data);
        }
    }
}
