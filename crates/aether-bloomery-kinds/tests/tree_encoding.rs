//! Canonical tree encoding and decode-refuses for invalid names and paths.

use std::collections::BTreeMap;
use std::error::Error;

use aether_bloomery_kinds::{Name, Node, Path, Tree};
use aether_data::{Kind, Ref, Storage, StorageData, StorageError, artifact_digest};

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
    0x00, 0x61, 0x00, 0x00, 0x00, 0x00, 0x85, 0x87, 0xcb, 0xf8, 0x41, 0xe5, 0x94, 0xa6, 0x97, 0xe7, 0x03, 0x90, 0x2d,
    0x4a, 0x37, 0x59, 0xa0, 0xef, 0xd1, 0x68, 0xbc, 0x97, 0xbd, 0xab, 0xfd, 0xfb, 0xba, 0x1b, 0x48, 0x5d, 0x26, 0xc8,
    0x01, 0x00, 0x00, 0x00, 0x62, 0x01, 0x00, 0x00, 0x00, 0x08, 0x32, 0x41, 0x87, 0x13, 0x9d, 0x19, 0x81, 0xde, 0xd5,
    0xad, 0xab, 0x8a, 0x24, 0x9e, 0xb4, 0xc0, 0x91, 0xd0, 0xf2, 0xf5, 0x83, 0xcc, 0xbc, 0x51, 0xd8, 0x28, 0x92, 0x91,
    0x48, 0xea, 0xb8, 0x01, 0x00, 0x00, 0x00, 0x63, 0x02, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x2e, 0x2e, 0x2f,
    0x62, 0x69, 0x6e, 0x2f, 0x72, 0x75, 0x6e, 0x01, 0x00, 0x00, 0x00, 0x64, 0x03, 0x00, 0x00, 0x00, 0xa8, 0x79, 0xac,
    0x5e, 0x6d, 0x41, 0x92, 0x60, 0xac, 0x2f, 0x23, 0xde, 0x84, 0xd7, 0x86, 0x0d, 0x83, 0x90, 0xb5, 0x9c, 0x08, 0x29,
    0xdc, 0xf7, 0x44, 0x87, 0xfd, 0xa7, 0x5a, 0x3b, 0x4e, 0x10,
];
const TRIPWIRE_TREE_DIGEST: [u8; 32] = [
    0xe5, 0x9c, 0xe1, 0x32, 0xf4, 0x44, 0x24, 0xb5, 0x39, 0xf8, 0x9d, 0x69, 0xdc, 0x06, 0x08, 0xea, 0x42, 0xd7, 0x23,
    0x96, 0xa6, 0x6f, 0x79, 0xe5, 0xd8, 0x55, 0xcf, 0x8f, 0x51, 0xc5, 0xce, 0xa6,
];
