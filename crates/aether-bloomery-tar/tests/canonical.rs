//! The canonical encoding: pinned bytes, the round trip, the depth cap, and
//! the encoder's handling of its blob readers and its output.

mod common;

use std::io::{self, ErrorKind, Read, Write};

use aether_bloomery_kinds::{Node, OpaqueBytes, Path, Ref, Tree, hash_bytes};
use aether_bloomery_tar::{EncodeError, MAX_DEPTH, SourceBlob, TreeSource, encode};
use common::MemoryStore;

/// The canonical tar stream of `root`.
fn canonical_bytes(store: &mut MemoryStore, root: &Ref<Tree>) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode(root, store, &mut bytes).expect("the tree encodes");
    bytes
}

/// A file, an executable, a symlink, an empty directory, a nested directory,
/// a 150-byte path, and a non-ASCII name.
fn fixture(store: &mut MemoryStore) -> Ref<Tree> {
    let run = Node::Executable(store.add_blob(b"#!/bin/sh\necho run\n"));
    let bin = Node::Directory(store.add_dir(vec![("run.sh", run)]));
    let empty = Node::Directory(store.add_dir(vec![]));
    let hello = Node::File(store.add_blob(b"hello\n"));
    let cafe = Node::File(store.add_blob("café\n".as_bytes()));
    let link = Node::Symlink(Path::new("hello.txt").expect("valid target"));
    let long_file = Node::File(store.add_blob(b"long\n"));
    let long_leaf = format!("{}.txt", "f".repeat(80));
    let long_dir = Node::Directory(store.add_dir(vec![(long_leaf.as_str(), long_file)]));
    let long = Node::Directory(store.add_dir(vec![("d".repeat(60).as_str(), long_dir)]));
    store.add_dir(vec![
        ("bin", bin),
        ("café.txt", cafe),
        ("empty", empty),
        ("hello.txt", hello),
        ("link", link),
        ("long", long),
    ])
}

#[test]
fn the_canonical_bytes_of_one_fixed_tree_are_pinned() {
    // Tripwire: every canonical rule at once — entry order, header fields,
    // owners, modes, mtime, checksum, the PAX rule for long and non-ASCII
    // paths, padding, and the end blocks. A drift in any of them changes the
    // bytes a workspace run hands the container, so the digest breaks.
    let mut store = MemoryStore::default();
    let root = fixture(&mut store);

    assert_eq!(hash_bytes(&canonical_bytes(&mut store, &root)).to_string(), TRIPWIRE_DIGEST);
}

const TRIPWIRE_DIGEST: &str = "d1ee62ddefb0daf74b9794d391e1f178db7014cac2e98359bc0f4cb50538452d";

#[test]
fn decode_inverts_encode() {
    // Catches an asymmetry between the writer and the reader: a PAX record
    // one side drops, a padding miscount, a blob cut at a copy-buffer
    // boundary, or a sibling order that differs from the tree's. Equal
    // references already make re-encoding the decoded tree reproduce the
    // canonical bytes, so that is not asserted separately.
    let mut store = MemoryStore::default();
    let fixture = fixture(&mut store);
    let big = Node::File(store.add_blob(&(0..150_000u32).map(|index| index.to_le_bytes()[0]).collect::<Vec<_>>()));
    let far = Node::Symlink(Path::new(format!("../{}", "t/".repeat(60) + "target")).expect("valid target"));
    let root = store.add_dir(vec![("big.bin", big), ("far", far), ("fixture", Node::Directory(fixture))]);

    let canonical = canonical_bytes(&mut store, &root);
    let decoded = store.decode(&canonical);

    assert_eq!(decoded, root);
}

#[test]
fn depth_is_capped_at_max_depth_segments() {
    // Catches an off-by-one in the encoder's depth check, and a decoder that
    // refuses the deepest tree the encoder accepts. The decoder's refusal
    // one level deeper is in the crate's refusal table.
    let mut store = MemoryStore::default();
    let mut chain = store.add_dir(vec![]);
    for _ in 1..MAX_DEPTH {
        chain = store.add_dir(vec![("a", Node::Directory(chain))]);
    }
    let deepest = store.add_dir(vec![("a", Node::Directory(chain))]);
    let too_deep = store.add_dir(vec![("a", Node::Directory(deepest))]);

    let canonical = canonical_bytes(&mut store, &deepest);
    assert_eq!(store.decode(&canonical), deepest);

    let result = encode(&too_deep, &mut store, Vec::new());
    assert!(matches!(result, Err(EncodeError::TooDeep)), "got {result:?}");
}

/// Serves one tree for every lookup and one blob with a stated length that
/// may be off, through a reader that reports `Interrupted` before every read.
struct OneBlobSource {
    tree: Tree,
    bytes: Vec<u8>,
    stated: u64,
}

impl TreeSource for OneBlobSource {
    type Error = String;
    type Blob<'a> = Interrupting<'a>;

    fn tree(&mut self, _tree: &Ref<Tree>) -> Result<Tree, String> {
        Ok(self.tree.clone())
    }

    fn blob(&mut self, _blob: &Ref<OpaqueBytes>) -> Result<SourceBlob<Interrupting<'_>>, String> {
        Ok(SourceBlob { len: self.stated, reader: Interrupting { bytes: self.bytes.as_slice(), interrupt: true } })
    }
}

/// A reader over `bytes` whose every other call fails with `Interrupted`.
struct Interrupting<'a> {
    bytes: &'a [u8],
    interrupt: bool,
}

impl Read for Interrupting<'_> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        self.interrupt = !self.interrupt;
        if !self.interrupt {
            return Err(ErrorKind::Interrupted.into());
        }
        self.bytes.read(into)
    }
}

/// A one-file tree in `store` and a [`OneBlobSource`] over the same file
/// that states `stated` bytes.
fn one_file(store: &mut MemoryStore, bytes: &[u8], stated: u64) -> (Ref<Tree>, Ref<OpaqueBytes>, OneBlobSource) {
    let blob = store.add_blob(bytes);
    let root = store.add_dir(vec![("f", Node::File(blob))]);
    let tree = TreeSource::tree(store, &root).expect("stored");
    (root, blob, OneBlobSource { tree, bytes: bytes.to_vec(), stated })
}

#[test]
fn an_interrupted_blob_read_is_retried() {
    // Catches an encoder that treats a spurious `Interrupted` from a blob
    // reader, in the copy or in the long-reader probe, as a failed read.
    let mut store = MemoryStore::default();
    let bytes = b"hello\n";
    let (root, _, mut source) = one_file(&mut store, bytes, bytes.len() as u64);

    let mut interrupted = Vec::new();
    encode(&root, &mut source, &mut interrupted).expect("interrupted reads are retried");

    assert_eq!(interrupted, canonical_bytes(&mut store, &root));
}

#[test]
fn a_reader_shorter_or_longer_than_its_stated_length_is_refused() {
    // Catches an encoder that trusts the stated length and writes a header
    // whose size disagrees with the bytes after it: a corrupt archive.
    let mut store = MemoryStore::default();
    let bytes = b"hello\n";
    let len = bytes.len() as u64;

    for (stated, actual) in [(len + 1, len), (len - 1, len)] {
        let (root, file, mut source) = one_file(&mut store, bytes, stated);
        match encode(&root, &mut source, Vec::new()) {
            Err(EncodeError::BlobLength { blob, expected, actual: got }) => {
                assert_eq!((blob, expected, got), (file, stated, actual));
            }
            other => panic!("stated {stated}: expected BlobLength, got {other:?}"),
        }
    }
}

/// Takes every write and fails every flush.
struct FailingFlush(Vec<u8>);

impl Write for FailingFlush {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("flush failed"))
    }
}

#[test]
fn a_failed_flush_of_the_output_is_an_error() {
    // Catches an encoder that returns Ok while the end blocks may still sit
    // in a buffered writer, the way a dropped `BufWriter` loses them.
    let mut store = MemoryStore::default();
    let root = fixture(&mut store);

    let result = encode(&root, &mut store, FailingFlush(Vec::new()));

    assert!(matches!(&result, Err(EncodeError::Write(error)) if error.to_string() == "flush failed"), "got {result:?}");
}
