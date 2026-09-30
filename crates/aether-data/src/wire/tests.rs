//! Conformance + roundtrip tests for the aether wire format (ADR-0118).
//!
//! Golden byte vectors pin the encoding to the ADR table (the authoritative
//! check until step 2 adds the adapter-vs-schema-walker cross-check); roundtrips
//! confirm the serializer and deserializer mirror each other.
#![allow(clippy::unwrap_used)]

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

use super::owned::take_array;
use super::{
    DecodeCtx, Decoder, Encoder, Error, HeldClaim, HeldLedger, InCtx, LedgerEncoder, WireDecode, WireEncode,
    decode_from_slice, encode_to_vec, from_bytes, take_from_bytes, to_vec,
};
use crate::ids::KindId;

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
    assert_eq!(to_vec(&1.5f32).unwrap()[..], 1.5f32.to_le_bytes()[..], "f32 is its IEEE bits, little-endian");
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
struct Blob {
    #[serde(with = "crate::bytes")]
    payload: Vec<u8>,
}

// Tripwire: the wire layout of a `Bytes` field is the `u32` little-endian
// count then the raw byte run — and the `serialize_bytes` fast path must emit
// exactly the bytes the per-element seq path emits for the same data, or the
// fast path is a silent format break for every peer still on the seq path.
#[test]
fn bytes_field_is_count_then_raw_and_matches_the_seq_path() {
    let blob = Blob { payload: vec![9, 8, 7] };
    let fast = to_vec(&blob).unwrap();
    assert_eq!(fast, vec![3, 0, 0, 0, 9, 8, 7]);
    assert_eq!(fast, to_vec(&vec![9u8, 8, 7]).unwrap(), "fast path and seq path diverge on the same payload");
}

#[test]
fn bytes_fast_path_and_seq_path_decode_each_other() {
    let blob = Blob { payload: vec![0, 1, 254, 255] };
    assert_eq!(from_bytes::<Vec<u8>>(&to_vec(&blob).unwrap()).unwrap(), blob.payload);
    assert_eq!(from_bytes::<Blob>(&to_vec(&blob.payload).unwrap()).unwrap(), blob);
}

#[test]
fn bytes_fast_path_roundtrips_a_large_buffer() {
    let payload: Vec<u8> = (0u32..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let blob = Blob { payload };
    let bytes = to_vec(&blob).unwrap();
    assert_eq!(bytes.len(), 4 + blob.payload.len());
    assert_eq!(from_bytes::<Blob>(&bytes).unwrap(), blob);
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
    assert_eq!(from_bytes::<BTreeMap<u8, u8>>(&to_vec(&map).unwrap()).unwrap(), map);
}

#[test]
fn typed_ids_are_eight_le_bytes() {
    let id = KindId(0x0102_0304_0506_0708);
    assert_eq!(to_vec(&id).unwrap()[..], id.0.to_le_bytes()[..]);
    assert_eq!(from_bytes::<KindId>(&to_vec(&id).unwrap()).unwrap().0, id.0);
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

/// The reply a test [`Debt`] answers.
const DEBT_REPLY: KindId = KindId(0x2A);

/// A test-only held leaf shaped as ADR-0243's `Held`: it writes its ticket
/// only through [`Encoder::held`] and reads it back only through
/// [`Decoder::claim_held`].
#[derive(Debug, PartialEq, Eq)]
struct Debt {
    ticket: u64,
}

impl WireEncode for Debt {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.encode_to(out)
    }

    fn encode_to<E: Encoder + ?Sized>(&self, enc: &mut E) -> Result<(), Error> {
        enc.held(self.ticket, DEBT_REPLY)
    }
}

impl<'de> WireDecode<'de> for Debt {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Self::decode_from(cursor)
    }

    fn decode_from<D: Decoder<'de> + ?Sized>(dec: &mut D) -> Result<Self, Error> {
        let ticket = u64::from_le_bytes(take_array::<8>(dec.cursor())?);
        let marker: u64 = dec.claim_held(ticket, DEBT_REPLY)?.downcast().unwrap();
        assert_eq!(marker, ticket, "the ledger hands back what it parked for this ticket");
        Ok(Self { ticket })
    }
}

/// A ledger that records every parked ticket and hands the ticket back as its
/// claim.
#[derive(Default)]
struct Ledger {
    parked: Vec<(u64, KindId)>,
}

impl HeldLedger for Ledger {
    fn park(&mut self, ticket: u64, reply: KindId) -> Result<(), Error> {
        self.parked.push((ticket, reply));
        Ok(())
    }

    fn claim(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, Error> {
        let at = self.parked.iter().position(|parked| *parked == (ticket, reply));
        let at = at.ok_or(Error::HeldUnclaimed { ticket, reply })?;
        self.parked.remove(at);
        Ok(HeldClaim(Box::new(ticket)))
    }
}

// Catches a default `held` or `claim_held` that silently writes or reads the
// ticket: a stray encode through a plain buffer would then leave a ticket in
// bytes no ledger parked, defusing the debt it carries.
#[test]
fn held_ticket_refuses_without_a_ledger() {
    let debt = Debt { ticket: 7 };
    let ungranted = Err(Error::HeldUngranted { reply: DEBT_REPLY });

    let mut out = Vec::new();
    assert_eq!(debt.encode_to(&mut out), ungranted);
    assert!(out.is_empty(), "a refused held ticket writes nothing");
    assert_eq!(encode_to_vec(&debt).map(|_| ()), ungranted);

    let bytes = 7u64.to_le_bytes();
    assert_eq!(decode_from_slice::<Debt>(&bytes), ungranted.map(|()| debt));

    let mut ctx = DecodeCtx::empty();
    assert_eq!(Debt::decode_from(&mut InCtx::new(&bytes, &mut ctx)), Err(Error::HeldUngranted { reply: DEBT_REPLY }));
}

// Catches a ledger encoder that writes without parking, parks without
// writing, or a granted decode that fails to claim the parked ticket back
// (or claims it twice).
#[test]
fn held_ticket_parks_through_a_ledger_encoder_and_claims_through_the_context() {
    let mut ledger = Ledger::default();
    let mut enc = LedgerEncoder::new(&mut ledger);
    Debt { ticket: 7 }.encode_to(&mut enc).unwrap();
    let bytes = enc.into_bytes();

    assert_eq!(bytes, 7u64.to_le_bytes());
    assert_eq!(ledger.parked, [(7, DEBT_REPLY)]);

    let mut ctx = DecodeCtx::empty().held(&mut ledger);
    assert_eq!(Debt::decode_from(&mut InCtx::new(&bytes, &mut ctx)), Ok(Debt { ticket: 7 }));
    assert_eq!(
        Debt::decode_from(&mut InCtx::new(&bytes, &mut ctx)),
        Err(Error::HeldUnclaimed { ticket: 7, reply: DEBT_REPLY }),
    );
    assert!(ledger.parked.is_empty());
}

/// Bytes of a schema `levels` deep: `open` per level, the `0u32` leaf
/// (`Unit` / `Anonymous`), then `close` per level on the way back out.
fn schema_chain(open: &[u32], close: &[u8], levels: usize) -> Vec<u8> {
    let open: Vec<u8> = open.iter().flat_map(|word| word.to_le_bytes()).collect();
    let mut bytes = open.repeat(levels);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&close.repeat(levels));
    bytes
}

// Catches a schema decode with no depth cap (the 100_000-level chain
// overflows the test thread's stack), a cap off by one against
// `aether-codec`'s walks, and a struct field, enum variant, or label child
// that restarts the depth at 0 and slips its chain past the cap.
#[test]
fn a_schema_nested_past_the_cap_refuses_instead_of_overflowing() {
    use core::fmt::Debug;

    use crate::{LabelNode, MAX_SCHEMA_DEPTH, SchemaShape, SchemaType};

    fn check<T: for<'de> WireDecode<'de> + Debug>(open: &[u32], close: &[u8]) {
        assert!(decode_from_slice::<T>(&schema_chain(open, close, MAX_SCHEMA_DEPTH)).is_ok());
        for levels in [MAX_SCHEMA_DEPTH + 1, 100_000] {
            assert_eq!(decode_from_slice::<T>(&schema_chain(open, close, levels)).unwrap_err(), Error::SchemaTooDeep);
        }
    }

    // An `Option` level: selector 5.
    check::<SchemaType>(&[5], &[]);
    // A one-field struct level: selector 8, one field, empty name; `repr_c` after the field.
    check::<SchemaType>(&[8, 1, 0], &[0]);
    // A one-variant enum level: selector 9, one variant, tuple selector 1,
    // empty name, discriminant 0, one field.
    check::<SchemaType>(&[9, 1, 1, 0, 0, 1], &[]);
    check::<SchemaShape>(&[5], &[]);
    // A label `Option` level: selector 1.
    check::<LabelNode>(&[1], &[]);
}
