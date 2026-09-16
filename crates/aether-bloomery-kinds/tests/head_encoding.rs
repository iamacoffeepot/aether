//! Canonical encoding of one fixed head-moved event, and decode-refuses
//! for an invalid symbol.

use std::error::Error;

use aether_bloomery_kinds::{Digest, Head, HeadMoved, Ref, Symbol, Tree, artifact_digest};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Kind, KindId, Storage, StorageData, StorageError};

fn fixture_event() -> HeadMoved {
    Head::<Tree>::new(Symbol::new("main").expect("valid symbol"))
        .move_to(Ref::from_digest(Digest::from_bytes([0x11; 32])))
        .into_event()
}

#[test]
fn the_canonical_encoding_of_one_fixed_head_moved_is_pinned() -> Result<(), Box<dyn Error>> {
    // Tripwire: the canonical encoding of a generic head move and the digest
    // it would take as an artifact. Catches drift in field tags, KindId
    // leaves, Symbol/string framing, Digest bytes, or the artifact prefix,
    // which would silently re-identify every stored head-moved event.
    let payload = HeadMoved::encode_storage(&StorageData::from_value(fixture_event()))?;
    let digest = artifact_digest(HeadMoved::ID, &payload);
    assert_eq!(payload, TRIPWIRE_HEAD_MOVED_PAYLOAD, "payload={payload:?}");
    assert_eq!(digest.as_bytes(), &TRIPWIRE_HEAD_MOVED_DIGEST, "digest={digest}");
    Ok(())
}

#[test]
fn decode_refuses_an_invalid_symbol_on_storage_and_wire() -> Result<(), Box<dyn Error>> {
    // Catches a Symbol decode path that skips check, truncates, or repairs
    // invalid bytes instead of refusing. The twin puts a raw String in the
    // same field position as HeadMoved.symbol.
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "bloomery.head_moved")]
    struct Twin {
        target_kind: KindId,
        symbol: String,
        to: Digest,
    }

    let twin = Twin { target_kind: Tree::ID, symbol: " ".into(), to: Digest::from_bytes([0x11; 32]) };
    let bytes = Twin::encode_storage(&StorageData::from_value(twin.clone()))?;
    match HeadMoved::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Symbol", reason: "whitespace" }) => {}
        other => panic!("expected Symbol whitespace invariant, got {other:?}"),
    }

    let mut wire = Vec::new();
    twin.encode(&mut wire)?;
    match HeadMoved::decode(&mut wire.as_slice()) {
        Err(WireError::Message(message)) if message == "whitespace" => Ok(()),
        other => panic!("expected wire Message whitespace, got {other:?}"),
    }
}

#[test]
fn decode_refuses_an_invalid_symbol_inside_a_container() -> Result<(), Box<dyn Error>> {
    // Catches a constructor bypass on the container element path: a Vec of
    // raw strings encodes, then Vec<Symbol> assemble_element skips check.
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.bloomery.head.symbols")]
    struct Plain {
        symbols: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.bloomery.head.symbols")]
    struct Checked {
        symbols: Vec<Symbol>,
    }

    let bytes = Plain::encode_storage(&StorageData::from_value(Plain { symbols: vec![" ".into()] }))?;
    match Checked::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Symbol", reason: "whitespace" }) => Ok(()),
        other => panic!("expected Symbol whitespace invariant, got {other:?}"),
    }
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
