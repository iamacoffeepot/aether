//! Byte-identity between the owned codec and the serde adapter.
//!
//! While both drivers exist, every corpus value must satisfy
//! `encode_to_vec(v) == to_vec(v)`, and each decoder must accept the
//! other's output. Golden arrays pin the agreed bytes so the pins survive
//! the adapter's later removal.

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Debug;

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::owned::{WireDecode, WireEncode, decode_from_slice, encode_to_vec};
use super::{from_bytes, to_vec};
use crate::mail::MailId;
use crate::schema::{
    EnumVariant, KindLabels, KindShape, LabelNode, NamedField, Primitive, ReplyContract, SchemaCell, SchemaShape,
    SchemaType, VariantLabel, VariantShape,
};
use crate::wire_id::{EngineId, SessionToken, Uuid};
use crate::{KindId, MailboxId};

fn owned_bytes<T: WireEncode>(value: &T) -> Vec<u8> {
    match encode_to_vec(value) {
        Ok(bytes) => bytes,
        Err(error) => panic!("owned encode: {error}"),
    }
}

fn serde_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    match to_vec(value) {
        Ok(bytes) => bytes,
        Err(error) => panic!("serde encode: {error}"),
    }
}

fn assert_drivers_agree<T>(value: &T)
where
    T: WireEncode + for<'de> WireDecode<'de> + Serialize + DeserializeOwned + PartialEq + Debug,
{
    let derived = owned_bytes(value);
    let adapter = serde_bytes(value);
    assert_eq!(derived, adapter, "owned codec and serde adapter diverged");

    let from_derived: T = match from_bytes(&derived) {
        Ok(decoded) => decoded,
        Err(error) => panic!("serde decode of owned bytes: {error}"),
    };
    assert_eq!(&from_derived, value);
    let from_adapter: T = match decode_from_slice(&adapter) {
        Ok(decoded) => decoded,
        Err(error) => panic!("owned decode of serde bytes: {error}"),
    };
    assert_eq!(&from_adapter, value);
}

fn assert_golden<T>(value: &T, golden: &[u8])
where
    T: WireEncode + for<'de> WireDecode<'de> + Serialize + DeserializeOwned + PartialEq + Debug,
{
    assert_drivers_agree(value);
    assert_eq!(owned_bytes(value), golden, "owned bytes drifted from the pinned encoding");
}

#[test]
fn scalars_match_serde_and_golden() {
    // Tripwire: little-endian fixed width. A swapped byte order or a varint
    // would move every content address that seals a scalar.
    assert_golden(&0x0403_0201u32, &[1, 2, 3, 4]);
    assert_golden(&7u8, &[7]);
    assert_golden(&(-1i16), &[0xFF, 0xFF]);
    assert_golden(&true, &[1]);
    assert_golden(&false, &[0]);
}

#[test]
fn string_option_vec_unit_match_serde_and_golden() {
    // Tripwire: u32 little-endian count then payload; option is a presence
    // byte; unit is zero bytes. A tagged encoding would split every address.
    assert_golden(&String::from("hi"), &[2, 0, 0, 0, b'h', b'i']);
    assert_golden(&Some(7u8), &[1, 7]);
    assert_golden(&Option::<u8>::None, &[0]);
    assert_golden(&vec![1u8, 2, 3], &[3, 0, 0, 0, 1, 2, 3]);
    assert_golden(&(), &[]);
}

#[test]
fn empty_collections_match_serde_and_golden() {
    // Tripwire: empty Vec / String / Map are a zero count, not omitted.
    assert_golden(&String::new(), &[0, 0, 0, 0]);
    assert_golden(&Vec::<u8>::new(), &[0, 0, 0, 0]);
    assert_golden(&Vec::<String>::new(), &[0, 0, 0, 0]);
    assert_golden(&BTreeMap::<u8, u8>::new(), &[0, 0, 0, 0]);
}

#[test]
fn map_u32_keys_sort_by_encoded_little_endian_bytes() {
    // Tripwire: keys 1 and 256 sort numerically as 1 < 256 but by
    // little-endian encoding as `[0,1,0,0] < [1,0,0,0]`. Walking the
    // BTreeMap in iteration order would mint a different address.
    let mut map = BTreeMap::new();
    map.insert(1u32, 10u8);
    map.insert(256u32, 20u8);
    let mut golden = Vec::new();
    golden.extend_from_slice(&2u32.to_le_bytes());
    golden.extend_from_slice(&256u32.to_le_bytes());
    golden.push(20);
    golden.extend_from_slice(&1u32.to_le_bytes());
    golden.push(10);
    assert_golden(&map, &golden);
}

#[test]
fn map_string_keys_of_unequal_length_match_serde() {
    let mut map = BTreeMap::new();
    map.insert(String::from("b"), 1u8);
    map.insert(String::from("aa"), 2u8);
    assert_drivers_agree(&map);
}

#[test]
fn engine_session_and_mail_id_match_serde() {
    let id = EngineId(Uuid::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]));
    assert_drivers_agree(&id);
    assert_drivers_agree(&SessionToken(id.0));
    assert_drivers_agree(&MailId::new(MailboxId(7), 9));
}

#[test]
fn typed_ids_match_serde_and_golden() {
    // Tripwire: typed ids are the bare u64, little-endian. A tagged-string
    // encoding on the binary path would renumber every Kind::ID consumer.
    let id = KindId(0x0102_0304_0506_0708);
    assert_golden(&id, &id.0.to_le_bytes());
    assert_drivers_agree(&MailboxId(7));
}

#[test]
fn nested_option_enum_sequence_matches_serde() {
    let value: Vec<Option<ReplyContract>> = vec![None, Some(ReplyContract::None), Some(ReplyContract::One(KindId(1)))];
    assert_drivers_agree(&value);
}

#[test]
fn schema_type_unit_and_bool_match_serde_and_golden() {
    // Tripwire: SchemaType selectors are declaration-index u32s. Drift here
    // is a Kind::ID renumber of every kind in the workspace.
    assert_golden(&SchemaType::Unit, &[0, 0, 0, 0]);
    assert_golden(&SchemaType::Bool, &[1, 0, 0, 0]);
    assert_golden(&SchemaType::String, &[3, 0, 0, 0]);
    assert_golden(&SchemaType::Bytes, &[4, 0, 0, 0]);
}

#[test]
fn schema_type_scalar_and_type_id_match_serde() {
    assert_drivers_agree(&SchemaType::Scalar(Primitive::F32));
    assert_drivers_agree(&SchemaType::TypeId(KindId::TYPE_ID));
}

#[test]
fn schema_cell_static_and_owned_encode_identically() {
    static INNER: SchemaType = SchemaType::Bool;
    let static_cell = SchemaCell::Static(&INNER);
    let owned_cell = SchemaCell::owned(SchemaType::Bool);
    assert_eq!(owned_bytes(&static_cell), owned_bytes(&owned_cell));
    assert_drivers_agree(&owned_cell);
    let decoded: SchemaCell = match decode_from_slice(&owned_bytes(&static_cell)) {
        Ok(cell) => cell,
        Err(error) => panic!("owned decode of static cell: {error}"),
    };
    assert!(matches!(decoded, SchemaCell::Owned(_)));
}

#[test]
fn schema_struct_both_repr_c_flags_match_serde() {
    let structured = SchemaType::Struct {
        fields: Cow::Owned(vec![NamedField { name: Cow::Borrowed("x"), ty: SchemaType::Scalar(Primitive::F32) }]),
        repr_c: false,
    };
    let cast = SchemaType::Struct {
        fields: Cow::Owned(vec![NamedField { name: Cow::Borrowed("x"), ty: SchemaType::Scalar(Primitive::F32) }]),
        repr_c: true,
    };
    assert_drivers_agree(&structured);
    assert_drivers_agree(&cast);
}

#[test]
fn schema_enum_unit_tuple_struct_variants_match_serde() {
    let schema = SchemaType::Enum {
        variants: Cow::Owned(vec![
            EnumVariant::Unit { name: Cow::Borrowed("Pending"), discriminant: 0 },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Ok"),
                discriminant: 1,
                fields: Cow::Owned(vec![SchemaType::Scalar(Primitive::U64)]),
            },
            EnumVariant::Struct {
                name: Cow::Borrowed("Err"),
                discriminant: 2,
                fields: Cow::Owned(vec![NamedField { name: Cow::Borrowed("reason"), ty: SchemaType::String }]),
            },
        ]),
    };
    assert_drivers_agree(&schema);
}

#[test]
fn schema_option_vec_array_map_match_serde() {
    assert_drivers_agree(&SchemaType::Option(SchemaCell::owned(SchemaType::Bool)));
    assert_drivers_agree(&SchemaType::Vec(SchemaCell::owned(SchemaType::String)));
    assert_drivers_agree(&SchemaType::Array { element: SchemaCell::owned(SchemaType::Scalar(Primitive::U32)), len: 3 });
    assert_drivers_agree(&SchemaType::Map {
        key: SchemaCell::owned(SchemaType::String),
        value: SchemaCell::owned(SchemaType::Scalar(Primitive::U8)),
    });
}

#[test]
fn schema_shape_and_kind_shape_match_serde() {
    let shape = SchemaShape::Struct {
        fields: vec![SchemaShape::Scalar(Primitive::F32), SchemaShape::Scalar(Primitive::F32)],
        repr_c: true,
    };
    assert_drivers_agree(&shape);
    assert_drivers_agree(&KindShape { name: Cow::Borrowed("test.vertex"), schema: shape });
    assert_drivers_agree(&VariantShape::Tuple { discriminant: 1, fields: vec![SchemaShape::Scalar(Primitive::U64)] });
}

#[test]
fn kind_labels_and_reply_contract_match_serde() {
    let labels = KindLabels {
        kind_id: KindId(0),
        kind_label: Cow::Borrowed("test.Vertex"),
        root: LabelNode::Struct {
            type_label: Some(Cow::Borrowed("test.Vertex")),
            field_names: Cow::Borrowed(&[Cow::Borrowed("x"), Cow::Borrowed("y")]),
            fields: Cow::Borrowed(&[LabelNode::Anonymous, LabelNode::Anonymous]),
        },
    };
    assert_drivers_agree(&labels);
    assert_drivers_agree(&ReplyContract::None);
    assert_drivers_agree(&ReplyContract::One(KindId(1)));
    assert_drivers_agree(&ReplyContract::Multi(KindId(2)));
    assert_drivers_agree(&ReplyContract::Manual);
    assert_drivers_agree(&VariantLabel::Unit { name: Cow::Borrowed("Pending") });
}

#[test]
fn float_bits_are_faithful_across_drivers() {
    let nan = f64::from_bits(0x7ff8_0000_0000_0001);
    let derived = owned_bytes(&nan);
    let adapter = serde_bytes(&nan);
    assert_eq!(derived, adapter);
    let back: f64 = match decode_from_slice(&derived) {
        Ok(value) => value,
        Err(error) => panic!("owned decode of nan bits: {error}"),
    };
    assert_eq!(back.to_bits(), nan.to_bits());
}
