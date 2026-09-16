//! Canonical encoding of one fixed head-moved event, and decode-refuses
//! for an invalid symbol.

use std::error::Error;

use aether_bloomery_kinds::{
    Digest, HeadMoved, Program, RecordedHeadMove, RecordedSymbol, Ref, Symbol, Tree, artifact_digest,
};
use aether_data::storage::{decode_derived, encode_derived};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Kind, KindId, Storage, StorageData, StorageError};

const MAIN: Symbol<Tree> = Symbol::new("main");

fn fixture_event() -> HeadMoved<Tree> {
    MAIN.move_to(Ref::from_digest(Digest::from_bytes([0x11; 32])))
}

#[test]
fn the_canonical_encoding_of_one_fixed_head_moved_is_pinned() -> Result<(), Box<dyn Error>> {
    // Tripwire: the canonical encoding of a generic head move and the digest
    // it would take as an artifact. Catches drift in field tags, KindId
    // leaves, Symbol/string framing, Digest bytes, or the artifact prefix,
    // which would silently re-identify every stored head-moved event.
    let payload = HeadMoved::<Tree>::encode_storage(&StorageData::from_value(fixture_event()))?;
    let digest = artifact_digest(HeadMoved::<Tree>::ID, &payload);
    assert_eq!(payload, TRIPWIRE_HEAD_MOVED_PAYLOAD, "payload={payload:?}");
    assert_eq!(digest.as_bytes(), &TRIPWIRE_HEAD_MOVED_DIGEST, "digest={digest}");

    let recorded = RecordedHeadMove::decode_storage(&payload)?;
    assert_eq!(recorded.value.symbol().as_str(), "main");
    assert_eq!(recorded.value.symbol().kind(), Tree::ID);
    assert_eq!(recorded.value.to(), Digest::from_bytes([0x11; 32]));
    assert_eq!(HeadMoved::<Tree>::decode_storage(&payload)?.value, fixture_event());
    match HeadMoved::<Program>::decode_storage(&payload) {
        Err(StorageError::Invariant { kind: "Symbol", reason: "kind-mismatch" }) => {}
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
fn decode_refuses_an_invalid_symbol_on_storage_and_wire() -> Result<(), Box<dyn Error>> {
    // Catches a Symbol decode path that skips check, truncates, or repairs
    // invalid bytes instead of refusing. The twin puts a raw String in the
    // same field position as the flattened event symbol.
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "bloomery.head_moved")]
    struct Twin {
        target_kind: KindId,
        symbol: String,
        to: Digest,
    }

    let twin = Twin { target_kind: Tree::ID, symbol: " ".into(), to: Digest::from_bytes([0x11; 32]) };
    let bytes = Twin::encode_storage(&StorageData::from_value(twin.clone()))?;
    match HeadMoved::<Tree>::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Symbol", reason: "whitespace" }) => {}
        other => panic!("expected Symbol whitespace invariant, got {other:?}"),
    }
    match RecordedHeadMove::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Symbol", reason: "whitespace" }) => {}
        other => panic!("expected recorded Symbol whitespace invariant, got {other:?}"),
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
fn decode_refuses_an_invalid_symbol_inside_a_container() -> Result<(), Box<dyn Error>> {
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
        symbol: String,
        to: Digest,
    }

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.bloomery.head.events")]
    struct Checked {
        events: Vec<HeadMoved<Tree>>,
    }

    let bytes = Plain::encode_storage(&StorageData::from_value(Plain {
        events: vec![RawEvent { target_kind: Tree::ID, symbol: " ".into(), to: Digest::from_bytes([0x11; 32]) }],
    }))?;
    match Checked::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Symbol", reason: "whitespace" }) => Ok(()),
        other => panic!("expected Symbol whitespace invariant, got {other:?}"),
    }
}

#[test]
fn standalone_symbols_encode_kind_and_name() -> Result<(), Box<dyn Error>> {
    // Catches a standalone symbol that still encoded as a bare string, so a
    // recorded identity could not round-trip kind plus name.
    const MAIN: Symbol<Tree> = Symbol::new("main");
    let recorded = RecordedSymbol::new(Tree::ID, "main")?;
    let typed_bytes = encode_derived(&StorageData::from_value(MAIN))?;
    let recorded_bytes = encode_derived(&StorageData::from_value(recorded.clone()))?;
    assert_eq!(typed_bytes, recorded_bytes);

    let typed = decode_derived::<Symbol<Tree>>(&typed_bytes, false)?;
    let as_recorded = decode_derived::<RecordedSymbol>(&recorded_bytes, false)?;
    assert_eq!(typed.value, MAIN);
    assert_eq!(as_recorded.value, recorded);
    assert_eq!(typed.value.kind(), Tree::ID);
    assert_eq!(as_recorded.value.kind(), Tree::ID);
    assert_eq!(typed.get::<KindId>("kind").transpose()?, Some(Tree::ID));
    assert_eq!(typed.get::<String>("name").transpose()?, Some(String::from("main")));

    match decode_derived::<Symbol<Program>>(&typed_bytes, false) {
        Err(StorageError::Invariant { kind: "Symbol", reason: "kind-mismatch" }) => Ok(()),
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
        symbol: String,
        to: Digest,
        extra: String,
    }

    let newer = WithExtra {
        target_kind: Tree::ID,
        symbol: "main".into(),
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
    assert_eq!(recorded.value.symbol().as_str(), "main");
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
    assert_eq!(recorded.symbol().kind(), Tree::ID);
    assert_eq!(recorded.to(), Digest::from_bytes([0x11; 32]));
    Ok(())
}

const TRIPWIRE_HEAD_MOVED_PAYLOAD: &[u8] = &[
    0x6d, 0x9e, 0x14, 0xe2, 0x8e, 0x9a, 0x69, 0x21, 0x08, 0x00, 0x00, 0x00, 0x1a, 0x6f, 0x35, 0x6a, 0x53, 0xdd, 0x19,
    0x24, 0x53, 0xab, 0x4a, 0x16, 0xeb, 0x9e, 0xf2, 0xdb, 0x20, 0x00, 0x00, 0x00, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
    0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
    0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x2e, 0x65, 0xfc, 0xf0, 0x95, 0xfb, 0x26, 0xe5, 0x08, 0x00, 0x00, 0x00,
    0x04, 0x00, 0x00, 0x00, 0x6d, 0x61, 0x69, 0x6e,
];
const TRIPWIRE_HEAD_MOVED_DIGEST: [u8; 32] = [
    0x69, 0x2b, 0xc7, 0xce, 0x5c, 0x0d, 0x9b, 0x07, 0xd0, 0x07, 0xa8, 0x19, 0x69, 0xc2, 0x74, 0xde, 0x6a, 0x33, 0x0f,
    0x5e, 0x1e, 0xfa, 0x7d, 0xbc, 0xd5, 0xc8, 0xaa, 0x96, 0xa0, 0x57, 0xb2, 0x84,
];
