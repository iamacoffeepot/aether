//! Canonical encoding of one fixed head-moved event, and decode-refuses
//! for an invalid head name.

use std::error::Error;

use aether_bloomery_kinds::{
    Digest, Head, HeadMoved, Program, RecordedHead, RecordedHeadMove, Ref, Tree, artifact_digest,
};
use aether_data::storage::{decode_derived, encode_derived};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Kind, KindId, Storage, StorageData, StorageError};

const MAIN: Head<Tree> = Head::new("main");

fn fixture_event() -> HeadMoved<Tree> {
    MAIN.move_to(Ref::from_digest(Digest::from_bytes([0x11; 32])))
}

#[test]
fn the_canonical_encoding_of_one_fixed_head_moved_is_pinned() -> Result<(), Box<dyn Error>> {
    // Tripwire: the canonical encoding of a generic head move and the digest
    // it would take as an artifact. Catches drift in field tags, KindId
    // leaves, `head`/string framing, Digest bytes, or the artifact prefix,
    // which would silently re-identify every stored head-moved event.
    let payload = HeadMoved::<Tree>::encode_storage(&StorageData::from_value(fixture_event()))?;
    let digest = artifact_digest(HeadMoved::<Tree>::ID, &payload);
    assert_eq!(payload, TRIPWIRE_HEAD_MOVED_PAYLOAD, "payload={payload:?}");
    assert_eq!(digest.as_bytes(), &TRIPWIRE_HEAD_MOVED_DIGEST, "digest={digest}");

    let recorded = RecordedHeadMove::decode_storage(&payload)?;
    assert_eq!(recorded.value.head().as_str(), "main");
    assert_eq!(recorded.value.head().kind(), Tree::ID);
    assert_eq!(recorded.value.to(), Digest::from_bytes([0x11; 32]));
    assert_eq!(HeadMoved::<Tree>::decode_storage(&payload)?.value, fixture_event());
    match HeadMoved::<Program>::decode_storage(&payload) {
        Err(StorageError::Invariant { kind: "Head", reason: "kind-mismatch" }) => {}
        other => panic!("expected typed kind-mismatch, got {other:?}"),
    }

    let mut wire = Vec::new();
    fixture_event().encode(&mut wire)?;
    match HeadMoved::<Program>::decode(&mut wire.as_slice()) {
        Err(WireError::Message(message)) if message == "kind-mismatch" => {}
        other => panic!("expected wire kind-mismatch, got {other:?}"),
    }
    Ok(())
}

#[test]
fn decode_refuses_an_invalid_head_name_on_storage_and_wire() -> Result<(), Box<dyn Error>> {
    // Catches a Head decode path that skips check, truncates, or repairs
    // invalid bytes instead of refusing. The twin puts a raw String in the
    // same field position as the flattened event `head` leaf.
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "bloomery.head_moved")]
    struct Twin {
        target_kind: KindId,
        head: String,
        to: Digest,
    }

    let twin = Twin { target_kind: Tree::ID, head: " ".into(), to: Digest::from_bytes([0x11; 32]) };
    let bytes = Twin::encode_storage(&StorageData::from_value(twin.clone()))?;
    match HeadMoved::<Tree>::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Head", reason: "whitespace" }) => {}
        other => panic!("expected Head whitespace invariant, got {other:?}"),
    }
    match RecordedHeadMove::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Head", reason: "whitespace" }) => {}
        other => panic!("expected recorded Head whitespace invariant, got {other:?}"),
    }

    let mut wire = Vec::new();
    twin.encode(&mut wire)?;
    match HeadMoved::<Tree>::decode(&mut wire.as_slice()) {
        Err(WireError::Message(message)) if message == "whitespace" => {}
        other => panic!("expected wire Message whitespace, got {other:?}"),
    }
    match RecordedHeadMove::decode(&mut wire.as_slice()) {
        Err(WireError::Message(message)) if message == "whitespace" => Ok(()),
        other => panic!("expected recorded wire Message whitespace, got {other:?}"),
    }
}

#[test]
fn decode_refuses_an_invalid_head_name_inside_a_container() -> Result<(), Box<dyn Error>> {
    // Catches a constructor bypass on the container element path: a Vec of
    // flattened twins encodes, then Vec<HeadMoved<Tree>> assemble_element
    // skips check.
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.bloomery.head.events")]
    struct Plain {
        events: Vec<RawEvent>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    struct RawEvent {
        target_kind: KindId,
        head: String,
        to: Digest,
    }

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.bloomery.head.events")]
    struct Checked {
        events: Vec<HeadMoved<Tree>>,
    }

    let bytes = Plain::encode_storage(&StorageData::from_value(Plain {
        events: vec![RawEvent { target_kind: Tree::ID, head: " ".into(), to: Digest::from_bytes([0x11; 32]) }],
    }))?;
    match Checked::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Head", reason: "whitespace" }) => Ok(()),
        other => panic!("expected Head whitespace invariant, got {other:?}"),
    }
}

#[test]
fn standalone_heads_encode_kind_and_name() -> Result<(), Box<dyn Error>> {
    // Catches a standalone head that still encoded as a bare string, so a
    // recorded identity could not round-trip kind plus name.
    const MAIN: Head<Tree> = Head::new("main");
    let recorded = RecordedHead::new(Tree::ID, "main")?;
    let typed_bytes = encode_derived(&StorageData::from_value(MAIN))?;
    let recorded_bytes = encode_derived(&StorageData::from_value(recorded.clone()))?;
    assert_eq!(typed_bytes, recorded_bytes);

    let typed = decode_derived::<Head<Tree>>(&typed_bytes, false)?;
    let as_recorded = decode_derived::<RecordedHead>(&recorded_bytes, false)?;
    assert_eq!(typed.value, MAIN);
    assert_eq!(as_recorded.value, recorded);
    assert_eq!(typed.value.kind(), Tree::ID);
    assert_eq!(as_recorded.value.kind(), Tree::ID);
    assert_eq!(typed.get::<KindId>("kind").transpose()?, Some(Tree::ID));
    assert_eq!(typed.get::<String>("name").transpose()?, Some(String::from("main")));

    match decode_derived::<Head<Program>>(&typed_bytes, false) {
        Err(StorageError::Invariant { kind: "Head", reason: "kind-mismatch" }) => Ok(()),
        other => panic!("expected standalone kind-mismatch, got {other:?}"),
    }
}

#[test]
fn unknown_fields_round_trip_and_remain_gettable() -> Result<(), Box<dyn Error>> {
    // Catches an adapter that decoded through a DTO conversion and dropped
    // StorageData unknown fields / records, so extra leaves vanished on
    // re-encode and get/get_raw could not see them.
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "bloomery.head_moved")]
    struct WithExtra {
        target_kind: KindId,
        head: String,
        to: Digest,
        extra: String,
    }

    let newer = WithExtra {
        target_kind: Tree::ID,
        head: "main".into(),
        to: Digest::from_bytes([0x11; 32]),
        extra: "keep".into(),
    };
    let produced = WithExtra::encode_storage(&StorageData::from_value(newer))?;
    let decoded = HeadMoved::<Tree>::decode_storage(&produced)?;
    assert_eq!(decoded.value, fixture_event());
    assert!(!decoded.unknown_fields.is_empty());
    assert_eq!(decoded.get::<String>("extra").transpose()?, Some(String::from("keep")));
    assert!(decoded.get_raw::<String>("extra").is_some());
    assert_eq!(HeadMoved::<Tree>::encode_storage(&decoded)?, produced);

    let recorded = RecordedHeadMove::decode_storage(&produced)?;
    assert_eq!(recorded.value.head().as_str(), "main");
    assert_eq!(RecordedHeadMove::encode_storage(&recorded)?, produced);
    Ok(())
}

#[test]
fn typed_and_recorded_events_are_interoperable() -> Result<(), Box<dyn Error>> {
    // Catches a typed codec that could not read recorded bytes, or a schema
    // split that gave HeadMoved<K> a different KindId than RecordedHeadMove.
    let typed_bytes = HeadMoved::<Tree>::encode_storage(&StorageData::from_value(fixture_event()))?;
    let recorded = RecordedHeadMove::decode_storage(&typed_bytes)?.value;
    let recorded_bytes = RecordedHeadMove::encode_storage(&StorageData::from_value(recorded.clone()))?;
    assert_eq!(typed_bytes, recorded_bytes);
    assert_eq!(HeadMoved::<Tree>::ID, RecordedHeadMove::ID);
    assert_eq!(HeadMoved::<Tree>::NAME, RecordedHeadMove::NAME);
    assert_eq!(HeadMoved::<Tree>::decode_storage(&recorded_bytes)?.value, fixture_event());
    assert_eq!(recorded.head().kind(), Tree::ID);
    assert_eq!(recorded.to(), Digest::from_bytes([0x11; 32]));
    Ok(())
}

const TRIPWIRE_HEAD_MOVED_PAYLOAD: &[u8] = &[
    0x6d, 0x9e, 0x14, 0xe2, 0x8e, 0x9a, 0x69, 0x21, 0x08, 0x00, 0x00, 0x00, 0x1a, 0x6f, 0x35, 0x6a, 0x53, 0xdd, 0x19,
    0x24, 0x64, 0xe5, 0xf9, 0x1e, 0xbd, 0xdf, 0xee, 0x99, 0x08, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x6d, 0x61,
    0x69, 0x6e, 0x53, 0xab, 0x4a, 0x16, 0xeb, 0x9e, 0xf2, 0xdb, 0x20, 0x00, 0x00, 0x00, 0x11, 0x11, 0x11, 0x11, 0x11,
    0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
    0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
];
const TRIPWIRE_HEAD_MOVED_DIGEST: [u8; 32] = [
    0xaf, 0xa1, 0xdc, 0x16, 0x1d, 0x95, 0x9d, 0x4a, 0xb1, 0xf7, 0xf4, 0xad, 0xe5, 0x64, 0xcf, 0x05, 0xf3, 0x77, 0x2a,
    0xca, 0x8a, 0x13, 0xd9, 0x03, 0x54, 0xd4, 0xba, 0x4d, 0x6a, 0x19, 0x80, 0x5e,
];
