//! Every refusal, driven by an archive crafted with the crate's own block and
//! PAX writers, plus the lenient inputs decode must still accept.

use std::collections::BTreeMap;
use std::convert::Infallible;

use aether_bloomery_kinds::{Name, NameError, Node, OpaqueBytes, PathError, Ref, Tree};

use super::{DecodeError, Refusal, decode};
use crate::MAX_DEPTH;
use crate::block::{BLOCK_BYTES, Header, padding_len, seal, typeflag};
use crate::pax;
use crate::store::{BlobWriter, TreeSink};

/// Hashes what it is handed and keeps nothing: these tests compare
/// references, never read a blob back.
struct HashingSink;

struct HashingBlob(Vec<u8>);

impl BlobWriter for HashingBlob {
    type Error = Infallible;

    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), Infallible> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(self) -> Result<Ref<OpaqueBytes>, Infallible> {
        Ok(Ref::of_bytes(&self.0))
    }
}

impl TreeSink for HashingSink {
    type Error = Infallible;
    type Blob<'a> = HashingBlob;

    fn begin_blob(&mut self, _len: u64) -> Result<HashingBlob, Infallible> {
        Ok(HashingBlob(Vec::new()))
    }

    fn put_tree(&mut self, tree: &Tree) -> Result<Ref<Tree>, Infallible> {
        Ok(Ref::of_encoded(tree).expect("a tree encodes"))
    }
}

fn header(name: &[u8], flag: u8, size: u64, link: &str) -> [u8; BLOCK_BYTES] {
    Header { name, typeflag: flag, mode: 0o644, size, linkname: link.as_bytes() }.to_block()
}

/// A header followed by its content and padding.
fn record(block: &[u8; BLOCK_BYTES], content: &[u8]) -> Vec<u8> {
    let mut bytes = block.to_vec();
    bytes.extend_from_slice(content);
    bytes.resize(bytes.len() + padding_len(content.len() as u64), 0);
    bytes
}

fn file(name: &str, content: &[u8]) -> Vec<u8> {
    record(&header(name.as_bytes(), typeflag::REGULAR, content.len() as u64, ""), content)
}

fn directory(name: &str) -> Vec<u8> {
    record(&header(name.as_bytes(), typeflag::DIRECTORY, 0, ""), b"")
}

fn link(name: &str, flag: u8, target: &str) -> Vec<u8> {
    record(&header(name.as_bytes(), flag, 0, target), b"")
}

fn pax_header(records: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (key, value) in records {
        pax::write_record(&mut body, key, value.as_bytes());
    }
    extended(typeflag::PAX, &body)
}

fn extended(flag: u8, body: &[u8]) -> Vec<u8> {
    record(&header(b"././@PaxHeader", flag, body.len() as u64, ""), body)
}

/// `parts` followed by the two end-of-archive blocks.
fn archive(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = parts.concat();
    bytes.resize(bytes.len() + 2 * BLOCK_BYTES, 0);
    bytes
}

fn refusal_of(bytes: &[u8]) -> (String, Refusal) {
    match decode(bytes, &mut HashingSink) {
        Err(DecodeError::Refused { entry, refusal }) => (entry, refusal),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn name(value: &str) -> Name {
    Name::new(value).expect("valid name")
}

fn tree(entries: Vec<(&str, Node)>) -> Ref<Tree> {
    let entries = entries.into_iter().map(|(key, node)| (name(key), node)).collect::<BTreeMap<_, _>>();
    Ref::of_encoded(&Tree::new(entries).expect("no collision")).expect("a tree encodes")
}

fn assert_refusals(cases: Vec<(&str, Vec<u8>, &str, Refusal)>) {
    for (case, bytes, entry, refusal) in cases {
        assert_eq!(refusal_of(&bytes), (entry.to_owned(), refusal), "{case}");
    }
}

#[test]
fn stream_refusals_are_driven_by_crafted_archives() {
    let mut bad_checksum = header(b"f", typeflag::REGULAR, 0, "");
    bad_checksum[0] = b'g';
    let mut unknown_magic = header(b"f", typeflag::REGULAR, 0, "");
    unknown_magic[257..263].copy_from_slice(b"xxxxx\0");
    seal(&mut unknown_magic);
    let mut bad_number = header(b"f", typeflag::REGULAR, 0, "");
    bad_number[124..136].copy_from_slice(b"0000000008\0\0");
    seal(&mut bad_number);
    let mut truncated_content = file("f", &[7; 100]);
    truncated_content.truncate(BLOCK_BYTES + 10);

    assert_refusals(vec![
        ("device node", archive(&[link("dev", b'3', "")]), "dev", Refusal::UnsupportedType(b'3')),
        ("fifo", archive(&[link("fifo", b'6', "")]), "fifo", Refusal::UnsupportedType(b'6')),
        (
            "pax global header",
            archive(&[extended(b'g', b""), file("f", b"")]),
            "././@PaxHeader",
            Refusal::UnsupportedType(b'g'),
        ),
        ("gnu sparse type", archive(&[link("s", b'S', "")]), "s", Refusal::UnsupportedType(b'S')),
        (
            "pax sparse record",
            archive(&[pax_header(&[("GNU.sparse.size", "9")]), file("f", b"")]),
            "././@PaxHeader",
            Refusal::Sparse,
        ),
        ("bad checksum", archive(&[record(&bad_checksum, b"")]), "g", Refusal::BadChecksum),
        ("unknown magic", archive(&[record(&unknown_magic, b"")]), "f", Refusal::UnknownFormat),
        ("bad number", archive(&[record(&bad_number, b"")]), "f", Refusal::BadNumber),
        (
            "oversized extended header",
            header(b"././@PaxHeader", typeflag::PAX, (1 << 20) + 1, "").to_vec(),
            "././@PaxHeader",
            Refusal::ExtendedTooLarge,
        ),
        (
            "repeated pax header",
            archive(&[pax_header(&[("path", "a")]), pax_header(&[("path", "b")]), file("f", b"")]),
            "././@PaxHeader",
            Refusal::ExtendedRepeated,
        ),
        (
            "repeated long name",
            archive(&[extended(b'L', b"a\0"), extended(b'L', b"b\0"), file("f", b"")]),
            "././@PaxHeader",
            Refusal::ExtendedRepeated,
        ),
        (
            "dangling extended header",
            archive(&[pax_header(&[("path", "a")])]),
            "././@PaxHeader",
            Refusal::ExtendedDangling,
        ),
        (
            "malformed pax record",
            archive(&[extended(typeflag::PAX, b"8 path=a\n"), file("f", b"")]),
            "././@PaxHeader",
            Refusal::ExtendedMalformed,
        ),
        ("empty input", Vec::new(), "", Refusal::Truncated),
        ("cut at a header boundary", file("f", b"x"), "", Refusal::Truncated),
        ("cut mid-content", truncated_content, "f", Refusal::Truncated),
    ]);
}

#[test]
fn entry_refusals_are_driven_by_crafted_archives() {
    let deep = vec!["a"; MAX_DEPTH + 1].join("/");

    assert_refusals(vec![
        (
            "not utf-8",
            archive(&[record(&header(b"\xff", typeflag::REGULAR, 0, ""), b"")]),
            "\u{fffd}",
            Refusal::NotUtf8,
        ),
        ("absolute path", archive(&[file("/etc/passwd", b"x")]), "/etc/passwd", Refusal::Absolute),
        ("parent segment", archive(&[file("a/../b", b"x")]), "a/../b", Refusal::ParentSegment),
        ("non-NFC name", archive(&[file("e\u{301}", b"x")]), "e\u{301}", Refusal::Name(NameError::NotNfc)),
        ("root as a file", archive(&[file(".", b"x")]), ".", Refusal::RootNotDirectory),
        ("too deep", archive(&[pax_header(&[("path", &deep)]), file("x", b"")]), &deep, Refusal::TooDeep),
        (
            "directory with content",
            archive(&[record(&header(b"d/", typeflag::DIRECTORY, 1, ""), b"x")]),
            "d/",
            Refusal::DirectorySize,
        ),
        (
            "absolute symlink target",
            archive(&[link("l", typeflag::SYMLINK, "/etc/passwd")]),
            "l",
            Refusal::LinkTarget(PathError::Absolute),
        ),
        ("dangling hardlink", archive(&[link("h", typeflag::HARDLINK, "missing")]), "h", Refusal::HardlinkTarget),
        (
            "hardlink to a directory",
            archive(&[directory("d/"), link("h", typeflag::HARDLINK, "d")]),
            "h",
            Refusal::HardlinkTarget,
        ),
        (
            "hardlink to a symlink",
            archive(&[link("l", typeflag::SYMLINK, "f"), link("h", typeflag::HARDLINK, "l")]),
            "h",
            Refusal::HardlinkTarget,
        ),
        ("same file twice", archive(&[file("a", b"1"), file("a", b"2")]), "a", Refusal::Duplicate),
        ("explicit directory twice", archive(&[directory("d/"), directory("d/")]), "d/", Refusal::Duplicate),
        ("file over an implicit directory", archive(&[file("d/f", b""), file("d", b"")]), "d", Refusal::Duplicate),
        ("entry under a file", archive(&[file("a", b""), file("a/b", b"")]), "a/b", Refusal::ParentNotDirectory),
    ]);
}

#[test]
fn lenient_inputs_decode_to_their_canonical_tree() {
    // Catches a dropped decode branch: GNU base-256 size, the signed
    // checksum, PAX over GNU `L` over the header name, the merge of an
    // explicit directory into an implicit one, and the root entry.
    let hello = Ref::of_bytes(b"hello!");

    let mut base256 = header(b"f", typeflag::REGULAR, 0, "");
    base256[124..136].copy_from_slice(&[0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6]);
    seal(&mut base256);

    let mut signed = header("é".as_bytes(), typeflag::REGULAR, 6, "");
    let signed_sum: i32 = signed
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if (148..156).contains(&index) {
                32
            } else {
                i32::from(byte.cast_signed())
            }
        })
        .sum();
    signed[148..156].copy_from_slice(format!("{signed_sum:06o}\0 ").as_bytes());

    let cases: Vec<(&str, Vec<u8>, Ref<Tree>)> = vec![
        ("base-256 size", archive(&[record(&base256, b"hello!")]), tree(vec![("f", Node::File(hello))])),
        ("signed checksum", archive(&[record(&signed, b"hello!")]), tree(vec![("é", Node::File(hello))])),
        (
            "pax path over gnu long name over header name",
            archive(&[extended(b'L', b"long\0"), pax_header(&[("path", "pax")]), file("plain", b"hello!")]),
            tree(vec![("pax", Node::File(hello))]),
        ),
        (
            "explicit directory after an implicit one",
            archive(&[file("./d/f", b"hello!"), directory("./d/"), directory("./")]),
            tree(vec![("d", Node::Directory(tree(vec![("f", Node::File(hello))])))]),
        ),
    ];

    for (case, bytes, expected) in cases {
        assert_eq!(decode(bytes.as_slice(), &mut HashingSink).expect(case), expected, "{case}");
    }
}
