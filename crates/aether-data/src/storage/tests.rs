//! Codec tests for the storage TLV shape. Version skew is testable in
//! one build by encoding with one Rust type and decoding with another
//! that shares a subset of leaves.

#![allow(clippy::unwrap_used)]

use alloc::string::String;
use alloc::vec::Vec;

use super::{
    RecordReader, RecordWriter, StorageData, StorageError, StorageLeaves, decode_derived, encode_derived, field_hash,
    field_path_root, fold_path_segment, variant_hash,
};
use crate::canonical::kind_id_from_parts;
use crate::hash::{KIND_DOMAIN, storage_kind_id_from_name};
use crate::ids::KindId;
use crate::schema::{NamedField, Primitive, SchemaType};
use crate::tagged_id::{Tag, with_tag};
use crate::{Schema, fnv1a_64_prefixed};
use alloc::borrow::Cow;

static U64_SCHEMA: SchemaType = SchemaType::Scalar(Primitive::U64);
static WIDE_SCHEMA: SchemaType = SchemaType::Struct {
    fields: Cow::Borrowed(&[
        NamedField { name: Cow::Borrowed("id"), ty: SchemaType::Scalar(Primitive::U64) },
        NamedField { name: Cow::Borrowed("note"), ty: SchemaType::String },
    ]),
    repr_c: false,
};

#[derive(Debug)]
struct Address {
    street: String,
    city: String,
}

impl StorageLeaves for Address {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.street.contribute(fold_path_segment(carry, b"street", depth), depth + 1, sink)?;
        self.city.contribute(fold_path_segment(carry, b"city", depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self {
            street: String::assemble(fold_path_segment(carry, b"street", depth), depth + 1, source)?,
            city: String::assemble(fold_path_segment(carry, b"city", depth), depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        String::is_absent(fold_path_segment(carry, b"street", depth), depth + 1, source)
            && String::is_absent(fold_path_segment(carry, b"city", depth), depth + 1, source)
    }
}

#[derive(Debug)]
struct V1 {
    id: u64,
    note: Option<String>,
}

impl StorageLeaves for V1 {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.id.contribute(fold_path_segment(carry, b"id", depth), depth + 1, sink)?;
        self.note.contribute(fold_path_segment(carry, b"note", depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self {
            id: u64::assemble(fold_path_segment(carry, b"id", depth), depth + 1, source)?,
            note: Option::<String>::assemble(fold_path_segment(carry, b"note", depth), depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        u64::is_absent(fold_path_segment(carry, b"id", depth), depth + 1, source)
            && Option::<String>::is_absent(fold_path_segment(carry, b"note", depth), depth + 1, source)
    }
}

#[derive(Debug)]
struct V2 {
    id: u64,
    note: Option<String>,
    extra: Option<u32>,
}

impl StorageLeaves for V2 {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.id.contribute(fold_path_segment(carry, b"id", depth), depth + 1, sink)?;
        self.note.contribute(fold_path_segment(carry, b"note", depth), depth + 1, sink)?;
        self.extra.contribute(fold_path_segment(carry, b"extra", depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self {
            id: u64::assemble(fold_path_segment(carry, b"id", depth), depth + 1, source)?,
            note: Option::<String>::assemble(fold_path_segment(carry, b"note", depth), depth + 1, source)?,
            extra: Option::<u32>::assemble(fold_path_segment(carry, b"extra", depth), depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        u64::is_absent(fold_path_segment(carry, b"id", depth), depth + 1, source)
            && Option::<String>::is_absent(fold_path_segment(carry, b"note", depth), depth + 1, source)
            && Option::<u32>::is_absent(fold_path_segment(carry, b"extra", depth), depth + 1, source)
    }
}

#[derive(Debug)]
struct Nested {
    id: u64,
    addr: Address,
    tags: Vec<String>,
}

impl StorageLeaves for Nested {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.id.contribute(fold_path_segment(carry, b"id", depth), depth + 1, sink)?;
        self.addr.contribute(fold_path_segment(carry, b"addr", depth), depth + 1, sink)?;
        self.tags.contribute(fold_path_segment(carry, b"tags", depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self {
            id: u64::assemble(fold_path_segment(carry, b"id", depth), depth + 1, source)?,
            addr: Address::assemble(fold_path_segment(carry, b"addr", depth), depth + 1, source)?,
            tags: Vec::<String>::assemble(fold_path_segment(carry, b"tags", depth), depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        u64::is_absent(fold_path_segment(carry, b"id", depth), depth + 1, source)
            && Address::is_absent(fold_path_segment(carry, b"addr", depth), depth + 1, source)
            && Vec::<String>::is_absent(fold_path_segment(carry, b"tags", depth), depth + 1, source)
    }
}

fn encode<T: StorageLeaves>(value: &T) -> Vec<u8> {
    let mut sink = RecordWriter::new();
    value.contribute(field_path_root(), 0, &mut sink).unwrap();
    sink.finish().unwrap()
}

#[test]
fn older_reader_reencode_of_newer_payload_is_byte_identical() {
    // If the writer emitted unsorted records, or the reader consumed an
    // unknown body's bytes as the next header, the older reader's
    // re-encode would drift from what the newer writer produced.
    let newer = V2 { id: 7, note: Some(String::from("hi")), extra: Some(9) };
    let produced = encode(&newer);
    let older: StorageData<V1> = decode_derived(&produced, false).unwrap();
    assert_eq!(older.value.id, 7);
    assert_eq!(older.value.note.as_deref(), Some("hi"));
    assert_eq!(older.unknown_fields.len(), 2);
    let reencoded = encode_derived(&older).unwrap();
    assert_eq!(reencoded, produced);
}

#[test]
fn newer_reader_sees_absent_optional_as_none() {
    let older = V1 { id: 3, note: None };
    let bytes = encode(&older);
    let newer: StorageData<V2> = decode_derived(&bytes, false).unwrap();
    assert_eq!(newer.value.id, 3);
    assert!(newer.value.note.is_none());
    assert!(newer.value.extra.is_none());
}

#[test]
fn missing_required_leaf_is_an_error() {
    let mut sink = RecordWriter::new();
    Option::<String>::contribute(&None, fold_path_segment(field_path_root(), b"note", 0), 1, &mut sink).unwrap();
    let bytes = sink.finish().unwrap();
    let err = decode_derived::<V1>(&bytes, false).unwrap_err();
    assert!(matches!(err, StorageError::MissingRequiredField { .. }));
}

#[test]
fn optional_absent_variant_yields_none() {
    let mut sink = RecordWriter::new();
    1u64.contribute(fold_path_segment(field_path_root(), b"id", 0), 1, &mut sink).unwrap();
    let bytes = sink.finish().unwrap();
    let decoded: StorageData<V1> = decode_derived(&bytes, false).unwrap();
    assert_eq!(decoded.value.id, 1);
    assert!(decoded.value.note.is_none());
}

#[test]
fn none_still_emits_variant_leaf() {
    // Discipline rule 5: wire-absence is version skew, never a sender
    // choosing to omit None. An explicit None must still write __variant.
    let bytes = encode(&V1 { id: 1, note: None });
    let decoded: StorageData<V1> = decode_derived(&bytes, false).unwrap();
    assert!(decoded.value.note.is_none());
    let var_hash = field_hash("note.__variant", &<u64 as Schema>::SCHEMA);
    assert!(decoded.get_raw::<u64>("note.__variant").is_some());
    let disc: u64 = decoded.get::<u64>("note.__variant").unwrap().unwrap();
    assert_eq!(disc, variant_hash("None", &super::UNIT_SCHEMA));
    let _ = var_hash;
}

#[test]
fn strict_mode_rejects_unknown_leaves() {
    let newer = V2 { id: 1, note: None, extra: Some(4) };
    let bytes = encode(&newer);
    let err = decode_derived::<V1>(&bytes, true).unwrap_err();
    assert!(matches!(err, StorageError::UnknownFieldInStrictMode { .. }));
}

#[test]
fn nested_struct_and_sequence_round_trip() {
    let value = Nested {
        id: 11,
        addr: Address { street: String::from("oak"), city: String::from("bend") },
        tags: Vec::from([String::from("a"), String::from("b")]),
    };
    let bytes = encode(&value);
    let decoded: StorageData<Nested> = decode_derived(&bytes, false).unwrap();
    assert_eq!(decoded.value.id, 11);
    assert_eq!(decoded.value.addr.street, "oak");
    assert_eq!(decoded.value.addr.city, "bend");
    assert_eq!(decoded.value.tags.len(), 2);
    assert_eq!(decoded.get::<String>("addr.street").unwrap().unwrap(), "oak");
}

#[test]
fn get_type_mismatch_misses_rather_than_misdecoding() {
    let value = V1 { id: 5, note: Some(String::from("x")) };
    let bytes = encode(&value);
    let decoded: StorageData<V1> = decode_derived(&bytes, false).unwrap();
    assert!(decoded.get::<String>("id").is_none());
    assert_eq!(decoded.get::<u64>("id").unwrap().unwrap(), 5);
}

#[test]
fn storage_kind_id_ignores_schema() {
    // Tripwire: storage ids hash KIND_DOMAIN + name. A mail kind's id
    // still moves with canonical schema bytes.
    let name = "persist.record";
    let expected = KindId(with_tag(Tag::Kind, fnv1a_64_prefixed(KIND_DOMAIN, name.as_bytes())));
    assert_eq!(storage_kind_id_from_name(name), expected);

    let mail_narrow = kind_id_from_parts(name, &U64_SCHEMA);
    let mail_wide = kind_id_from_parts(name, &WIDE_SCHEMA);
    assert_ne!(mail_narrow, mail_wide);
    assert_ne!(storage_kind_id_from_name(name).0, mail_wide);
}

#[test]
fn read_alias_does_not_appear_on_write() {
    #[derive(Debug)]
    struct Renamed {
        remark: Option<String>,
    }
    impl StorageLeaves for Renamed {
        fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
            self.remark.contribute(fold_path_segment(carry, b"remark", depth), depth + 1, sink)
        }
        fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
            let primary = fold_path_segment(carry, b"remark", depth);
            let alias = fold_path_segment(carry, b"note", depth);
            Ok(Self { remark: super::assemble_with_aliases(primary, &[alias], depth + 1, source)? })
        }
        fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
            Option::<String>::is_absent(fold_path_segment(carry, b"remark", depth), depth + 1, source)
        }
    }

    let old = V1 { id: 1, note: Some(String::from("keep")) };
    let old_bytes = encode(&old);
    // Drop the required `id` by decoding as renamed after stripping? The
    // alias test only needs the note leaves. Encode renamed and confirm
    // the write path uses `remark`, then read old `note` leaves.
    let new_bytes = encode(&Renamed { remark: Some(String::from("keep")) });
    let remark_hash = field_hash("remark.__variant", &<u64 as Schema>::SCHEMA);
    let note_hash = field_hash("note.__variant", &<u64 as Schema>::SCHEMA);
    assert!(RecordReader::parse(&new_bytes).unwrap().contains(remark_hash));
    assert!(!RecordReader::parse(&new_bytes).unwrap().contains(note_hash));

    let mut sink = RecordWriter::new();
    old.note.contribute(fold_path_segment(field_path_root(), b"note", 0), 1, &mut sink).unwrap();
    let aliased = decode_derived::<Renamed>(&sink.finish().unwrap(), false).unwrap();
    assert_eq!(aliased.value.remark.as_deref(), Some("keep"));
    let _ = old_bytes;
}
