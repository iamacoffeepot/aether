//! The blob rewrite over hand-built in-process payloads: each tag-1 field is
//! written as the envelope encoder writes it, a tag byte and a 32-byte hash.

use aether_data::{BlobHash, EnumVariant, NamedField, Primitive, SchemaCell, SchemaType};
use serde_json::json;

use super::{InlineError, inline_blobs};
use crate::decode_schema;
use crate::test_fixtures::{named, structured_struct};

const FIRST: BlobHash = BlobHash::from_bytes([0xa1; 32]);
const SECOND: BlobHash = BlobHash::from_bytes([0xb2; 32]);

fn write_count(out: &mut Vec<u8>, count: usize) {
    out.extend_from_slice(&u32::try_from(count).expect("test counts fit a u32").to_le_bytes());
}

fn write_string(out: &mut Vec<u8>, text: &str) {
    write_count(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

fn write_hash_field(out: &mut Vec<u8>, hash: BlobHash) {
    out.push(1);
    out.extend_from_slice(hash.as_bytes());
}

fn write_inline_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(0);
    write_count(out, bytes.len());
    out.extend_from_slice(bytes);
}

/// `{ head: String, maybe: Option<Blob>, list: Vec<Blob>, choice: Choice, tail: String }`
/// where `Choice` is `Empty | Wrapped(Blob) | Named { blob: Blob, mark: u16 }`.
fn nested_schema() -> SchemaType {
    let choice = SchemaType::Enum {
        variants: vec![
            EnumVariant::Unit { name: "Empty".into(), discriminant: 0 },
            EnumVariant::Tuple { name: "Wrapped".into(), discriminant: 1, fields: vec![SchemaType::Blob].into() },
            EnumVariant::Struct {
                name: "Named".into(),
                discriminant: 2,
                fields: vec![
                    NamedField { name: "blob".into(), ty: SchemaType::Blob },
                    NamedField { name: "mark".into(), ty: SchemaType::Scalar(Primitive::U16) },
                ]
                .into(),
            },
        ]
        .into(),
    };
    structured_struct(vec![
        named("head", SchemaType::String),
        named("maybe", SchemaType::Option(SchemaCell::owned(SchemaType::Blob))),
        named("list", SchemaType::Vec(SchemaCell::owned(SchemaType::Blob))),
        named("choice", choice),
        named("tail", SchemaType::String),
    ])
}

/// A `nested_schema` payload: `FIRST` in the option, `SECOND` then an inline
/// blob in the list, `FIRST` again in the enum's struct variant.
fn nested_payload() -> Vec<u8> {
    let mut out = Vec::new();
    write_string(&mut out, "head");
    out.push(1);
    write_hash_field(&mut out, FIRST);
    write_count(&mut out, 2);
    write_hash_field(&mut out, SECOND);
    write_inline_field(&mut out, b"xy");
    write_count(&mut out, 2);
    write_hash_field(&mut out, FIRST);
    out.extend_from_slice(&7u16.to_le_bytes());
    write_string(&mut out, "tail");
    out
}

fn attachments() -> [(BlobHash, &'static [u8]); 2] {
    [(FIRST, b"first bytes".as_slice()), (SECOND, b"2nd".as_slice())]
}

/// Every tag-1 field, wherever it nests, comes back as tag 0 carrying its
/// attachment's bytes, and every byte around it survives: the result decodes
/// through the outside codec, which refuses tag 1, to the expected value.
/// Catches a walker that loses its place after a variable-length field, or
/// that splices at the wrong offset.
#[test]
fn nested_hash_fields_become_inline_bytes() {
    let out = inline_blobs(&nested_schema(), &nested_payload(), &attachments(), usize::MAX).expect("rewrite succeeds");

    let decoded = decode_schema(&out, &nested_schema()).expect("result is tag-0 wire bytes");
    assert_eq!(
        decoded,
        json!({
            "head": "head",
            "maybe": b"first bytes".to_vec(),
            "list": [b"2nd".to_vec(), b"xy".to_vec()],
            "choice": { "Named": { "blob": b"first bytes".to_vec(), "mark": 7 } },
            "tail": "tail",
        })
    );
}

/// A payload with no tag-1 field comes back byte for byte: inline blobs,
/// blob-free shapes, and a sequence of zero-byte elements are copied through.
/// Catches a rewrite of fields that need none.
#[test]
fn payloads_without_hash_fields_come_back_unchanged() {
    let schema = structured_struct(vec![
        named("inline", SchemaType::Blob),
        named("units", SchemaType::Vec(SchemaCell::owned(SchemaType::Unit))),
        named(
            "pairs",
            SchemaType::Map { key: SchemaCell::owned(SchemaType::String), value: SchemaCell::owned(SchemaType::Bytes) },
        ),
    ]);
    let mut payload = Vec::new();
    write_inline_field(&mut payload, b"already inline");
    write_count(&mut payload, 3);
    write_count(&mut payload, 1);
    write_string(&mut payload, "key");
    write_string(&mut payload, "value");

    let out = inline_blobs(&schema, &payload, &attachments(), usize::MAX).expect("rewrite succeeds");

    assert_eq!(out, payload);
}

/// The limit is checked against the exact materialized size: at the size the
/// rewrite succeeds, one byte under it the rewrite refuses and names that size.
/// Catches an unbounded or misreported limit.
#[test]
fn limit_is_the_exact_materialized_size() {
    let schema = nested_schema();
    let payload = nested_payload();
    let size = inline_blobs(&schema, &payload, &attachments(), usize::MAX).expect("rewrite succeeds").len();

    let at_limit = inline_blobs(&schema, &payload, &attachments(), size).expect("a result at the limit fits");
    assert_eq!(at_limit.len(), size);

    let refused = inline_blobs(&schema, &payload, &attachments(), size - 1);
    assert!(
        matches!(refused, Err(InlineError::TooLarge { size: reported, limit }) if reported == size && limit == size - 1),
        "expected TooLarge naming {size} over {}, got {refused:?}",
        size - 1,
    );
}

/// A tag-1 hash that no attachment carries is refused naming that hash, even
/// when other attachments are present. Catches a panic on a stray tag 1, or a
/// lookup that settles for the wrong entry.
#[test]
fn a_hash_without_its_attachment_is_refused() {
    let only_second = [(SECOND, b"2nd".as_slice())];

    let refused = inline_blobs(&nested_schema(), &nested_payload(), &only_second, usize::MAX);

    assert!(
        matches!(refused, Err(InlineError::MissingAttachment { hash }) if hash == FIRST),
        "expected MissingAttachment for FIRST, got {refused:?}",
    );
}
