//! Canonical tree encoding and decode-refuses for invalid names and paths.

use std::collections::BTreeMap;
use std::error::Error;

use aether_bloomery_kinds::{Name, Node, Path, Ref, Tree, artifact_digest};
use aether_data::{Kind, Storage, StorageData, StorageError};

fn name(value: &str) -> Name {
    Name::new(value).expect("valid name")
}

fn path(value: &str) -> Path {
    Path::new(value).expect("valid path")
}

fn fixture_tree() -> Tree {
    let empty = Tree::empty();
    let mut entries = BTreeMap::new();
    entries.insert(name("a"), Node::File(Ref::of_bytes(b"hello")));
    entries.insert(name("b"), Node::Executable(Ref::of_bytes(b"#!/bin/sh")));
    entries.insert(name("c"), Node::Symlink(path("../bin/run")));
    entries.insert(name("d"), Node::Directory(Ref::of_encoded(&empty).expect("empty tree encodes")));
    Tree::new(entries)
}

fn encode_tree(tree: &Tree) -> Result<Vec<u8>, StorageError> {
    Tree::encode_storage(&StorageData::from_value(tree.clone()))
}

#[test]
fn the_canonical_encoding_of_one_fixed_tree_is_pinned() -> Result<(), Box<dyn Error>> {
    // Tripwire: the canonical encoding of a tree, entry order, and the digest
    // every stored tree in every journal depends on. Catches drift in field
    // tags, map encoding, variant discriminants, or the framing.
    let payload = encode_tree(&fixture_tree())?;
    let digest = artifact_digest(Tree::ID, &payload);
    assert_eq!(payload, TRIPWIRE_TREE_PAYLOAD);
    assert_eq!(digest.as_bytes(), &TRIPWIRE_TREE_DIGEST);
    Ok(())
}

#[test]
fn decode_refuses_an_invalid_name() -> Result<(), Box<dyn Error>> {
    // Catches a validated newtype whose decode path skips the check.
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.bloomery.tree_twin")]
    struct Twin {
        entries: BTreeMap<String, Node>,
    }

    let mut entries = BTreeMap::new();
    entries.insert("..".into(), Node::File(Ref::of_bytes(b"hello")));
    let bytes = Twin::encode_storage(&StorageData::from_value(Twin { entries }))?;
    match Tree::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Name", .. }) => Ok(()),
        other => panic!("expected Name invariant, got {other:?}"),
    }
}

#[test]
fn decode_refuses_an_invalid_path() -> Result<(), Box<dyn Error>> {
    // Catches a validated newtype whose decode path skips the check. The
    // symlink target "X" is one byte so the length prefix stays put when
    // that byte is replaced with NUL.
    let mut entries = BTreeMap::new();
    entries.insert(name("a"), Node::Symlink(path("X")));
    let mut bytes = encode_tree(&Tree::new(entries))?;
    let Some(index) = bytes.iter().position(|byte| *byte == b'X') else {
        panic!("fixture encoding did not contain the symlink target byte");
    };
    bytes[index] = 0;
    match Tree::decode_storage(&bytes) {
        Err(StorageError::Invariant { kind: "Path", .. }) => Ok(()),
        other => panic!("expected Path invariant, got {other:?}"),
    }
}

#[test]
fn decode_accepts_names_that_differ_only_in_case() -> Result<(), Box<dyn Error>> {
    // Catches a uniqueness check stricter than byte-exact map keys, such as
    // case folding, which refuses a Debian userland (`xt_CONNMARK.h` beside
    // `xt_connmark.h`).
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.bloomery.tree_twin")]
    struct Twin {
        entries: BTreeMap<String, Node>,
    }

    let mut entries = BTreeMap::new();
    entries.insert("xt_CONNMARK.h".into(), Node::File(Ref::of_bytes(b"a")));
    entries.insert("xt_connmark.h".into(), Node::File(Ref::of_bytes(b"b")));
    let bytes = Twin::encode_storage(&StorageData::from_value(Twin { entries }))?;

    let tree = Tree::decode_storage(&bytes)?.value;
    let names: Vec<&str> = tree.entries().keys().map(Name::as_str).collect();
    assert_eq!(names, ["xt_CONNMARK.h", "xt_connmark.h"]);
    Ok(())
}

const TRIPWIRE_TREE_PAYLOAD: &[u8] = &[
    0xe5, 0xc8, 0xde, 0xdf, 0x1c, 0x33, 0x10, 0x45, 0x96, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x61, 0x00, 0x00, 0x00, 0x00, 0xe5, 0x95, 0x01, 0x71, 0xbb, 0x75, 0x9e, 0xbb, 0x67, 0xe9, 0xd4, 0x79, 0x4e,
    0xf1, 0x75, 0x32, 0x75, 0x6c, 0xf7, 0x0d, 0xb5, 0xaf, 0xea, 0x7d, 0x00, 0xd1, 0x41, 0xba, 0x2a, 0x00, 0x74, 0xd8,
    0x01, 0x00, 0x00, 0x00, 0x62, 0x01, 0x00, 0x00, 0x00, 0xa9, 0x86, 0xc4, 0x15, 0x4d, 0x21, 0x10, 0x9f, 0x4a, 0xd8,
    0xc2, 0x43, 0x5d, 0x96, 0xfa, 0x05, 0xf5, 0xac, 0xfc, 0xcf, 0x29, 0x53, 0x77, 0xed, 0x9b, 0x34, 0xeb, 0x44, 0x02,
    0x79, 0x53, 0x43, 0x01, 0x00, 0x00, 0x00, 0x63, 0x02, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x2e, 0x2e, 0x2f,
    0x62, 0x69, 0x6e, 0x2f, 0x72, 0x75, 0x6e, 0x01, 0x00, 0x00, 0x00, 0x64, 0x03, 0x00, 0x00, 0x00, 0xa8, 0x79, 0xac,
    0x5e, 0x6d, 0x41, 0x92, 0x60, 0xac, 0x2f, 0x23, 0xde, 0x84, 0xd7, 0x86, 0x0d, 0x83, 0x90, 0xb5, 0x9c, 0x08, 0x29,
    0xdc, 0xf7, 0x44, 0x87, 0xfd, 0xa7, 0x5a, 0x3b, 0x4e, 0x10,
];
const TRIPWIRE_TREE_DIGEST: [u8; 32] = [
    0x3f, 0x65, 0x84, 0x21, 0x85, 0xf1, 0x67, 0x92, 0xd7, 0xb1, 0x3b, 0x10, 0x7b, 0x65, 0xe0, 0x8a, 0x8d, 0xd7, 0x5a,
    0x95, 0x0a, 0xce, 0x04, 0x4c, 0x75, 0xf6, 0xef, 0x05, 0x96, 0xa5, 0x74, 0x22,
];
