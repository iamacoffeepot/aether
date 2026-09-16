//! Kindless nested `Storage` and validated newtypes.
//!
//! Each test names the bug it catches. No derive-only roundtrips.

#![allow(clippy::unwrap_used)]

use std::borrow::Cow;

use aether_data::storage::{
    RecordReader, RecordWriter, StorageElement, StorageError, StorageLeaves, UNIT_SCHEMA, VARIANT_LEAF,
    field_path_root, fold_path_segment, variant_hash,
};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Invariant, NamedField, Schema, SchemaType, Storage, StorageData};

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
enum Shape {
    Off,
    Named { n: u32 },
    Pair(u32, String),
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.nested.shape_holder")]
struct Holder {
    shape: Shape,
}

#[derive(Debug, Clone, PartialEq)]
enum ShapeTwin {
    Off,
    Named { n: u32 },
    Pair(u32, String),
}

const NAMED_FIELDS: &[NamedField] = &[NamedField { name: Cow::Borrowed("n"), ty: <u32 as Schema>::SCHEMA }];
const PAIR_FIELDS: &[NamedField] = &[
    NamedField { name: Cow::Borrowed("0"), ty: <u32 as Schema>::SCHEMA },
    NamedField { name: Cow::Borrowed("1"), ty: <String as Schema>::SCHEMA },
];
const NAMED_SCHEMA: SchemaType = SchemaType::Struct { fields: Cow::Borrowed(NAMED_FIELDS), repr_c: false };
const PAIR_SCHEMA: SchemaType = SchemaType::Struct { fields: Cow::Borrowed(PAIR_FIELDS), repr_c: false };

impl StorageLeaves for ShapeTwin {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        let disc = match self {
            Self::Off => variant_hash("Off", &UNIT_SCHEMA),
            Self::Named { .. } => variant_hash("Named", &NAMED_SCHEMA),
            Self::Pair(..) => variant_hash("Pair", &PAIR_SCHEMA),
        };
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        u64::contribute(&disc, var_carry, depth + 1, sink)?;
        match self {
            Self::Off => Ok(()),
            Self::Named { n } => {
                let body = fold_path_segment(carry, b"Named", depth);
                let field = fold_path_segment(body, b"n", depth + 1);
                u32::contribute(n, field, depth + 2, sink)
            }
            Self::Pair(a, b) => {
                let body = fold_path_segment(carry, b"Pair", depth);
                let f0 = fold_path_segment(body, b"0", depth + 1);
                let f1 = fold_path_segment(body, b"1", depth + 1);
                u32::contribute(a, f0, depth + 2, sink)?;
                String::contribute(b, f1, depth + 2, sink)
            }
        }
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        let disc = u64::assemble(var_carry, depth + 1, source)?;
        if disc == variant_hash("Off", &UNIT_SCHEMA) {
            Ok(Self::Off)
        } else if disc == variant_hash("Named", &NAMED_SCHEMA) {
            let body = fold_path_segment(carry, b"Named", depth);
            let field = fold_path_segment(body, b"n", depth + 1);
            Ok(Self::Named { n: u32::assemble(field, depth + 2, source)? })
        } else if disc == variant_hash("Pair", &PAIR_SCHEMA) {
            let body = fold_path_segment(carry, b"Pair", depth);
            let f0 = fold_path_segment(body, b"0", depth + 1);
            let f1 = fold_path_segment(body, b"1", depth + 1);
            Ok(Self::Pair(u32::assemble(f0, depth + 2, source)?, String::assemble(f1, depth + 2, source)?))
        } else {
            Err(StorageError::UnknownVariant { hash: disc })
        }
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        u64::is_absent(var_carry, depth + 1, source)
    }
}

fn shape_carry() -> u64 {
    fold_path_segment(field_path_root(), b"shape", 0)
}

fn assemble_twin(bytes: &[u8]) -> ShapeTwin {
    let mut source = RecordReader::parse(bytes).unwrap();
    ShapeTwin::assemble(shape_carry(), 1, &mut source).unwrap()
}

fn encode_twin(twin: &ShapeTwin) -> Vec<u8> {
    let mut sink = RecordWriter::new();
    twin.contribute(shape_carry(), 1, &mut sink).unwrap();
    sink.finish().unwrap()
}

#[test]
fn kindless_enum_matches_hand_written_fault_layout() {
    // Catches the derive and the established hand-written layout disagreeing,
    // which is the whole point of swapping FaultReason onto the derive.
    let cases = [
        (Shape::Off, ShapeTwin::Off),
        (Shape::Named { n: 7 }, ShapeTwin::Named { n: 7 }),
        (Shape::Pair(3, String::from("x")), ShapeTwin::Pair(3, String::from("x"))),
    ];
    for (derived, twin) in cases {
        let produced = Holder::encode_storage(&StorageData::from_value(Holder { shape: derived.clone() })).unwrap();
        assert_eq!(assemble_twin(&produced), twin);

        let decoded = Holder::decode_storage(&encode_twin(&twin)).unwrap();
        assert_eq!(decoded.value.shape, derived);
    }
}

#[test]
fn unknown_nested_variant_is_unknown_variant_through_a_root() {
    // Catches a nested enum that surfaces an unknown discriminant as TrailingBytes
    // or MissingRequiredField instead of UnknownVariant.
    let mut sink = RecordWriter::new();
    let unknown = 0xdead_beef_u64;
    u64::contribute(&unknown, fold_path_segment(shape_carry(), VARIANT_LEAF.as_bytes(), 1), 2, &mut sink).unwrap();
    let err = Holder::decode_storage(&sink.finish().unwrap()).unwrap_err();
    assert!(matches!(err, StorageError::UnknownVariant { hash } if hash == unknown));
}

#[derive(Debug, PartialEq, aether_data::Storage)]
struct CellV1 {
    id: u32,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
struct CellV2 {
    id: u32,
    note: Option<String>,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.nested.bag")]
struct BagV1 {
    cells: Vec<CellV1>,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.nested.bag")]
struct BagV2 {
    cells: Vec<CellV2>,
}

#[test]
fn kindless_struct_vec_element_is_tagged_and_tolerates_dropped_fields() {
    // Catches emitting the positional element by mistake: that form moves the
    // container tag on schema drift and refuses instead of skipping the extra field.
    const {
        assert!(<CellV1 as StorageElement>::TAGGED);
        assert!(<CellV2 as StorageElement>::TAGGED);
    }
    let bytes = BagV2::encode_storage(&StorageData::from_value(BagV2 {
        cells: vec![CellV2 { id: 1, note: Some(String::from("keep")) }],
    }))
    .unwrap();
    let old = BagV1::decode_storage(&bytes).unwrap();
    assert_eq!(old.value.cells, vec![CellV1 { id: 1 }]);
}

struct TooLong;

impl Invariant for TooLong {
    fn reason(&self) -> &'static str {
        "too-long"
    }
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[storage(validate)]
struct ShortName(String);

impl ShortName {
    // The derive calls `T::check(&inner)` with `Inner` as the tuple field.
    #[allow(clippy::ptr_arg)]
    fn check(inner: &String) -> Result<(), TooLong> {
        if inner.len() > 3 {
            Err(TooLong)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.nested.validated")]
struct PlainRoot {
    name: String,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.nested.validated")]
struct CheckedRoot {
    name: ShortName,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.nested.validated_vec")]
struct PlainNames {
    names: Vec<String>,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "test.nested.validated_vec")]
struct CheckedNames {
    names: Vec<ShortName>,
}

#[test]
fn validated_newtype_refuses_on_every_decode_path() {
    // Catches a path that skips check: a root field, a Vec element, or WireDecode
    // accepting a value the constructor would refuse.
    let long = String::from("abcd");
    let err = CheckedRoot::decode_storage(
        &PlainRoot::encode_storage(&StorageData::from_value(PlainRoot { name: long.clone() })).unwrap(),
    )
    .unwrap_err();
    assert!(matches!(err, StorageError::Invariant { kind: "ShortName", reason: "too-long" }));

    let err = CheckedNames::decode_storage(
        &PlainNames::encode_storage(&StorageData::from_value(PlainNames { names: vec![long.clone()] })).unwrap(),
    )
    .unwrap_err();
    assert!(matches!(err, StorageError::Invariant { kind: "ShortName", reason: "too-long" }));

    let mut wire = Vec::new();
    long.encode(&mut wire).unwrap();
    let err = ShortName::decode(&mut wire.as_slice()).unwrap_err();
    assert!(matches!(err, WireError::Message(message) if message == "too-long"));
}
