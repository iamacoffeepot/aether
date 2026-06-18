//! Conformance + roundtrip tests for the aether wire format (ADR-0118).
//!
//! Golden byte vectors pin the encoding to the ADR table (the authoritative
//! check until step 2 adds the adapter-vs-schema-walker cross-check); roundtrips
//! confirm the serializer and deserializer mirror each other.
#![allow(clippy::unwrap_used)]

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

use super::{Error, from_bytes, take_from_bytes, to_vec};
use crate::ids::KindId;
use crate::{Kind, Ref};

#[test]
fn scalars_are_fixed_little_endian() {
    assert_eq!(to_vec(&0x0403_0201u32).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(to_vec(&7u8).unwrap(), vec![7]);
    assert_eq!(to_vec(&(-1i16)).unwrap(), vec![0xFF, 0xFF]);
}

#[test]
fn bool_is_one_byte() {
    assert_eq!(to_vec(&true).unwrap(), vec![1]);
    assert_eq!(to_vec(&false).unwrap(), vec![0]);
}

#[test]
fn float_is_bit_faithful() {
    assert_eq!(
        to_vec(&1.5f32).unwrap()[..],
        1.5f32.to_le_bytes()[..],
        "f32 is its IEEE bits, little-endian"
    );
    // A NaN payload survives unchanged (bit-faithful, no normalization).
    let nan = f64::from_bits(0x7ff8_0000_0000_0001);
    let back: f64 = from_bytes(&to_vec(&nan).unwrap()).unwrap();
    assert_eq!(back.to_bits(), nan.to_bits());
}

#[test]
fn string_is_u32_len_then_utf8() {
    assert_eq!(to_vec("hi").unwrap(), vec![2, 0, 0, 0, b'h', b'i']);
    let back: String = from_bytes(&to_vec("héllo").unwrap()).unwrap();
    assert_eq!(back, "héllo");
}

#[test]
fn option_is_a_presence_byte() {
    assert_eq!(to_vec(&Some(7u8)).unwrap(), vec![1, 7]);
    assert_eq!(to_vec(&Option::<u8>::None).unwrap(), vec![0]);
}

#[test]
fn vec_is_u32_count_then_elements() {
    assert_eq!(to_vec(&vec![1u8, 2, 3]).unwrap(), vec![3, 0, 0, 0, 1, 2, 3]);
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Point {
    x: i32,
    y: i32,
}

#[test]
fn struct_fields_are_positional() {
    let p = Point { x: 1, y: -1 };
    let mut body = Vec::new();
    body.extend_from_slice(&1i32.to_le_bytes());
    body.extend_from_slice(&(-1i32).to_le_bytes());
    assert_eq!(to_vec(&p).unwrap(), body);
    assert_eq!(from_bytes::<Point>(&to_vec(&p).unwrap()).unwrap(), p);
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
enum Shape {
    Dot,
    Circle(u32),
    Rect { w: u32, h: u32 },
}

#[test]
fn enum_selector_is_u32_then_body() {
    assert_eq!(to_vec(&Shape::Dot).unwrap(), vec![0, 0, 0, 0]);

    let mut circle = Vec::new();
    circle.extend_from_slice(&1u32.to_le_bytes());
    circle.extend_from_slice(&5u32.to_le_bytes());
    assert_eq!(to_vec(&Shape::Circle(5)).unwrap(), circle);

    for shape in [Shape::Dot, Shape::Circle(9), Shape::Rect { w: 2, h: 3 }] {
        let bytes = to_vec(&shape).unwrap();
        assert_eq!(from_bytes::<Shape>(&bytes).unwrap(), shape);
    }
}

/// Emits map entries in a deliberately unsorted order so the serializer's
/// canonical key-sort is exercised (a `BTreeMap` would already be sorted).
struct UnsortedMap(Vec<(u8, u8)>);

impl Serialize for UnsortedMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

#[test]
fn map_is_canonical_key_sorted() {
    let unsorted = UnsortedMap(vec![(3, 30), (1, 10), (2, 20)]);
    assert_eq!(
        to_vec(&unsorted).unwrap(),
        vec![3, 0, 0, 0, 1, 10, 2, 20, 3, 30],
        "entries emit in ascending key order regardless of insertion order"
    );

    let mut map = BTreeMap::new();
    map.insert(1u8, 10u8);
    map.insert(2u8, 20u8);
    assert_eq!(
        from_bytes::<BTreeMap<u8, u8>>(&to_vec(&map).unwrap()).unwrap(),
        map
    );
}

#[test]
fn typed_ids_are_eight_le_bytes() {
    let id = KindId(0x0102_0304_0506_0708);
    assert_eq!(to_vec(&id).unwrap()[..], id.0.to_le_bytes()[..]);
    assert_eq!(from_bytes::<KindId>(&to_vec(&id).unwrap()).unwrap().0, id.0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tiny(u32);

impl Kind for Tiny {
    const NAME: &'static str = "test.tiny";
    const ID: KindId = KindId(0xDEAD_BEEF);

    fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
        let arr: [u8; 4] = bytes.try_into().ok()?;
        Some(Self(u32::from_le_bytes(arr)))
    }

    fn encode_into_bytes(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

#[test]
fn ref_inline_is_selector_len_prefix_then_kind_image() {
    let r = Ref::<Tiny>::inline(Tiny(7));
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes()); // inline selector
    body.extend_from_slice(&4u32.to_le_bytes()); // length-prefixed body
    body.extend_from_slice(&7u32.to_le_bytes()); // Tiny::encode_into_bytes image
    assert_eq!(to_vec(&r).unwrap(), body);
    assert_eq!(from_bytes::<Ref<Tiny>>(&to_vec(&r).unwrap()).unwrap(), r);
}

#[test]
fn ref_handle_is_selector_then_ids() {
    let r = Ref::<Tiny>::handle(0xCAFE);
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_le_bytes()); // handle selector
    body.extend_from_slice(&0xCAFE_u64.to_le_bytes()); // id
    body.extend_from_slice(&Tiny::ID.0.to_le_bytes()); // kind_id
    assert_eq!(to_vec(&r).unwrap(), body);
    assert_eq!(from_bytes::<Ref<Tiny>>(&to_vec(&r).unwrap()).unwrap(), r);
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Rich {
    name: String,
    tags: Vec<String>,
    maybe: Option<u64>,
    nested: Point,
    flag: bool,
}

#[test]
fn nested_value_roundtrips_and_is_deterministic() {
    let r = Rich {
        name: "x".into(),
        tags: vec!["a".into(), "b".into()],
        maybe: Some(42),
        nested: Point { x: 5, y: 6 },
        flag: true,
    };
    let bytes = to_vec(&r).unwrap();
    assert_eq!(to_vec(&r).unwrap(), bytes, "encoding is deterministic");
    assert_eq!(from_bytes::<Rich>(&bytes).unwrap(), r);
}

#[test]
fn from_bytes_rejects_trailing_bytes() {
    assert_eq!(from_bytes::<u8>(&[1, 2]), Err(Error::TrailingBytes));
}

#[test]
fn truncated_input_is_unexpected_eof() {
    assert_eq!(from_bytes::<u32>(&[1, 2]), Err(Error::UnexpectedEof));
}

#[test]
fn invalid_bool_byte_is_rejected() {
    assert_eq!(from_bytes::<bool>(&[2]), Err(Error::InvalidBool(2)));
}

#[test]
fn take_from_bytes_returns_the_remainder() {
    let mut bytes = to_vec(&7u8).unwrap();
    bytes.extend_from_slice(&[0xAA, 0xBB]);
    let (value, rest): (u8, &[u8]) = take_from_bytes(&bytes).unwrap();
    assert_eq!(value, 7);
    assert_eq!(rest, &[0xAA, 0xBB]);
}

#[test]
fn take_from_bytes_walks_back_to_back_records() {
    // Two records concatenated decode in sequence, each handing back the
    // remainder — the shape the manifest reader relies on.
    let mut buf = to_vec(&7u8).unwrap();
    buf.extend_from_slice(&to_vec(&0x0102_0304u32).unwrap());
    let (first, rest): (u8, &[u8]) = take_from_bytes(&buf).unwrap();
    assert_eq!(first, 7);
    let (second, rest): (u32, &[u8]) = take_from_bytes(rest).unwrap();
    assert_eq!(second, 0x0102_0304);
    assert!(rest.is_empty());
}
