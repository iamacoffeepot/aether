//! Property-based round-trip coverage for the schema-driven codec
//! (issue 1561). The law under test is the single most load-bearing
//! invariant in the wire path: for any value valid under a schema,
//!
//! ```text
//! decode_schema(&encode_schema(&v, &s)?, &s)? == v
//! ```
//!
//! Two strategies cooperate. [`arb_schema`] generates a bounded
//! `SchemaType` tree (depth ≤ 4, collections ≤ 4 elements) over the
//! `Unit` / `Bool` / `Scalar` / `String` / `Bytes` / `Option` / `Vec` /
//! `Array` / `Struct` / `Enum` / `Map` arms. [`value_for_schema`] then
//! derives a matching `serde_json::Value` for a concrete schema, shaped
//! exactly as the decoder emits it (so the round-trip is byte-for-byte,
//! not merely structurally, equal).
//!
//! The two cast/postcard wire shapes both get coverage: a struct is
//! generated with `repr_c: true` only when every field is cast-eligible
//! (mirroring the encoder's own eligibility rule, [`cast_eligible`]), so
//! the generator produces both the `bytemuck`-cast image and the
//! postcard image. Values that the JSON layer cannot carry losslessly
//! are excluded at the source — floats are finite and `f32`-representable
//! (`serde_json` drops `NaN`/`Inf` to null), and `Map` keys honor the
//! proto3-style `String` / integer-scalar / `Bool` restriction.

#![cfg(test)]

use aether_data::{EnumVariant, Primitive, SchemaCell, SchemaType};
use proptest::prelude::*;
use proptest::strategy::Union;
use serde_json::{Map, Value};

use crate::test_fixtures::named;
use crate::{decode_schema, encode_schema};

/// Cap on generated collection / struct / enum widths. Keeps each case's
/// wire image small so 256 cases stay in the low-single-digit-seconds
/// budget the issue asks for.
const MAX_WIDTH: usize = 4;

/// Mirror of the encoder's cast-eligibility rule (`cast::non_cast_variant_error`
/// plus the recursive descent the cast walker does): a struct may set
/// `repr_c: true` only when every field is itself cast-eligible. `Scalar`
/// is a leaf yes; `Array` is eligible iff its element is; a nested
/// `Struct` is eligible iff it is itself `repr_c: true` (which, by
/// construction, already implies its own fields are eligible). Everything
/// else — `Bool`, `String`, `Bytes`, `Option`, `Vec`, `Enum`, `Map`,
/// `Unit` — disqualifies the parent. `Ref` / `TypeId` are never generated.
fn cast_eligible(ty: &SchemaType) -> bool {
    match ty {
        SchemaType::Scalar(_) | SchemaType::TypeId(_) => true,
        SchemaType::Array { element, .. } => cast_eligible(element),
        SchemaType::Struct { fields, repr_c } => {
            *repr_c && fields.iter().all(|f| cast_eligible(&f.ty))
        }
        _ => false,
    }
}

/// Every scalar primitive, uniformly.
fn arb_primitive() -> impl Strategy<Value = Primitive> {
    prop_oneof![
        Just(Primitive::U8),
        Just(Primitive::U16),
        Just(Primitive::U32),
        Just(Primitive::U64),
        Just(Primitive::I8),
        Just(Primitive::I16),
        Just(Primitive::I32),
        Just(Primitive::I64),
        Just(Primitive::F32),
        Just(Primitive::F64),
    ]
}

/// The proto3-style map-key restriction: keys are `String`, an integer
/// `Scalar`, or `Bool` — never a float or a composite.
fn arb_map_key_schema() -> impl Strategy<Value = SchemaType> {
    prop_oneof![
        Just(SchemaType::String),
        Just(SchemaType::Bool),
        prop_oneof![
            Just(Primitive::U8),
            Just(Primitive::U16),
            Just(Primitive::U32),
            Just(Primitive::U64),
            Just(Primitive::I8),
            Just(Primitive::I16),
            Just(Primitive::I32),
            Just(Primitive::I64),
        ]
        .prop_map(SchemaType::Scalar),
    ]
}

/// One generated enum variant before it is assigned a name + discriminant.
#[derive(Debug, Clone)]
enum VariantSpec {
    Unit,
    Tuple(Vec<SchemaType>),
    Struct(Vec<SchemaType>),
}

/// Assemble a `SchemaType::Enum` from generated variant specs, assigning
/// each a unique name (`V0`, `V1`, …) and a discriminant equal to its
/// index. Struct-variant fields get unique names (`f0`, `f1`, …) so the
/// decoder's object keys stay distinct.
fn build_enum(specs: Vec<VariantSpec>) -> SchemaType {
    let variants = specs
        .into_iter()
        .enumerate()
        .map(|(i, spec)| {
            let discriminant = u32::try_from(i).expect("variant index fits u32");
            let name = format!("V{i}");
            match spec {
                VariantSpec::Unit => EnumVariant::Unit {
                    name: name.into(),
                    discriminant,
                },
                VariantSpec::Tuple(fields) => EnumVariant::Tuple {
                    name: name.into(),
                    discriminant,
                    fields: fields.into(),
                },
                VariantSpec::Struct(fields) => EnumVariant::Struct {
                    name: name.into(),
                    discriminant,
                    fields: fields
                        .into_iter()
                        .enumerate()
                        .map(|(j, ty)| named(&format!("f{j}"), ty))
                        .collect::<Vec<_>>()
                        .into(),
                },
            }
        })
        .collect::<Vec<_>>();
    SchemaType::Enum {
        variants: variants.into(),
    }
}

/// A bounded `SchemaType` tree: leaves are the scalar/atom arms, and the
/// recursive layer adds `Option` / `Vec` / `Array` / `Struct` / `Enum` /
/// `Map`. Depth is capped at 4 so the generated wire images stay small.
fn arb_schema() -> impl Strategy<Value = SchemaType> {
    let leaf = prop_oneof![
        Just(SchemaType::Unit),
        Just(SchemaType::Bool),
        arb_primitive().prop_map(SchemaType::Scalar),
        Just(SchemaType::String),
        Just(SchemaType::Bytes),
    ];
    // depth ≤ 4, ~64 total nodes, ~MAX_WIDTH children per recursive node.
    leaf.prop_recursive(4, 64, 4, |inner| {
        let variant_spec = prop_oneof![
            Just(VariantSpec::Unit),
            prop::collection::vec(inner.clone(), 1..=MAX_WIDTH).prop_map(VariantSpec::Tuple),
            prop::collection::vec(inner.clone(), 1..=MAX_WIDTH).prop_map(VariantSpec::Struct),
        ];
        prop_oneof![
            inner
                .clone()
                .prop_map(|s| SchemaType::Option(SchemaCell::owned(s))),
            inner
                .clone()
                .prop_map(|s| SchemaType::Vec(SchemaCell::owned(s))),
            (inner.clone(), 0..=MAX_WIDTH).prop_map(|(s, len)| SchemaType::Array {
                element: SchemaCell::owned(s),
                len: u32::try_from(len).expect("array len fits u32"),
            }),
            (
                prop::collection::vec(inner.clone(), 0..=MAX_WIDTH),
                any::<bool>()
            )
                .prop_map(|(types, want_repr_c)| {
                    let fields = types
                        .into_iter()
                        .enumerate()
                        .map(|(i, ty)| named(&format!("f{i}"), ty))
                        .collect::<Vec<_>>();
                    let repr_c = want_repr_c && fields.iter().all(|f| cast_eligible(&f.ty));
                    SchemaType::Struct {
                        fields: fields.into(),
                        repr_c,
                    }
                }),
            prop::collection::vec(variant_spec, 1..=MAX_WIDTH).prop_map(build_enum),
            (arb_map_key_schema(), inner).prop_map(|(key, value)| SchemaType::Map {
                key: SchemaCell::owned(key),
                value: SchemaCell::owned(value),
            }),
        ]
    })
}

/// A finite scalar value shaped exactly as the decoder emits it: each
/// integer width round-trips through the same-width `Value::from` (so the
/// `PosInt` / `NegInt` `serde_json::Number` variant matches), and floats
/// are finite — `f32` additionally constrained to a value that survives
/// the `f64`-JSON round-trip by deriving it from a real `f32`.
fn arb_scalar_value(p: Primitive) -> BoxedStrategy<Value> {
    match p {
        Primitive::U8 => any::<u8>().prop_map(Value::from).boxed(),
        Primitive::U16 => any::<u16>().prop_map(Value::from).boxed(),
        Primitive::U32 => any::<u32>().prop_map(Value::from).boxed(),
        Primitive::U64 => any::<u64>().prop_map(Value::from).boxed(),
        Primitive::I8 => any::<i8>().prop_map(Value::from).boxed(),
        Primitive::I16 => any::<i16>().prop_map(Value::from).boxed(),
        Primitive::I32 => any::<i32>().prop_map(Value::from).boxed(),
        Primitive::I64 => any::<i64>().prop_map(Value::from).boxed(),
        Primitive::F32 => any::<f32>()
            .prop_filter("f32 must be finite", |f| f.is_finite())
            .prop_map(|f| Value::from(f64::from(f)))
            .boxed(),
        Primitive::F64 => any::<f64>()
            .prop_filter("f64 must be finite", |f| f.is_finite())
            .prop_map(Value::from)
            .boxed(),
    }
}

/// Fold a heterogeneous list of value strategies into one strategy
/// yielding the `Vec<Value>` of their joint samples. proptest has no
/// variadic tuple combinator past arity 12, and struct/tuple widths are
/// generator-chosen, so the join is built incrementally.
fn join_values(strats: Vec<BoxedStrategy<Value>>) -> BoxedStrategy<Vec<Value>> {
    let mut acc: BoxedStrategy<Vec<Value>> = Just(Vec::new()).boxed();
    for s in strats {
        acc = (acc, s)
            .prop_map(|(mut v, x)| {
                v.push(x);
                v
            })
            .boxed();
    }
    acc
}

/// Build a JSON object strategy from parallel `names` + per-field value
/// strategies.
fn join_object(names: Vec<String>, strats: Vec<BoxedStrategy<Value>>) -> BoxedStrategy<Value> {
    join_values(strats)
        .prop_map(move |vals| {
            let mut obj = Map::new();
            for (n, v) in names.iter().zip(vals) {
                obj.insert(n.clone(), v);
            }
            Value::Object(obj)
        })
        .boxed()
}

/// `{ name: body }` — the externally-tagged form the codec uses for
/// tuple / struct enum variants.
fn tagged(name: &str, body: Value) -> Value {
    let mut obj = Map::with_capacity(1);
    obj.insert(name.to_owned(), body);
    Value::Object(obj)
}

/// A JSON value for one enum variant, in the decoder's emitted shape:
/// unit → bare string tag; single-field tuple → unwrapped body; multi-
/// field tuple → array body; struct → object body.
fn value_for_variant(variant: &EnumVariant) -> BoxedStrategy<Value> {
    match variant {
        EnumVariant::Unit { name, .. } => {
            let name = name.to_string();
            Just(Value::String(name)).boxed()
        }
        EnumVariant::Tuple { name, fields, .. } => {
            let name = name.to_string();
            if fields.len() == 1 {
                value_for_schema(&fields[0])
                    .prop_map(move |v| tagged(&name, v))
                    .boxed()
            } else {
                let strats = fields.iter().map(value_for_schema).collect();
                join_values(strats)
                    .prop_map(move |vals| tagged(&name, Value::Array(vals)))
                    .boxed()
            }
        }
        EnumVariant::Struct { name, fields, .. } => {
            let name = name.to_string();
            let field_names = fields.iter().map(|f| f.name.to_string()).collect();
            let strats = fields.iter().map(|f| value_for_schema(&f.ty)).collect();
            join_object(field_names, strats)
                .prop_map(move |body| tagged(&name, body))
                .boxed()
        }
    }
}

/// A JSON object strategy for a `Map` schema. Keys are generated through
/// the native key type (so uniqueness is structural) and then stringified
/// into the canonical decimal / `"true"`-`"false"` / identity form the
/// decoder emits.
fn value_for_map(key_schema: &SchemaType, value_schema: &SchemaType) -> BoxedStrategy<Value> {
    let value_strat = value_for_schema(value_schema);
    match key_schema {
        SchemaType::String => prop::collection::btree_map(
            prop::string::string_regex("[a-zA-Z0-9_-]{0,8}").expect("static regex"),
            value_strat,
            0..=MAX_WIDTH,
        )
        .prop_map(|m| Value::Object(m.into_iter().collect()))
        .boxed(),
        SchemaType::Bool => prop::collection::btree_map(any::<bool>(), value_strat, 0..=2)
            .prop_map(|m| {
                Value::Object(
                    m.into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect::<Map<_, _>>(),
                )
            })
            .boxed(),
        SchemaType::Scalar(p) => {
            prop::collection::btree_map(arb_int_key(*p), value_strat, 0..=MAX_WIDTH)
                .prop_map(|m| {
                    Value::Object(
                        m.into_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect::<Map<_, _>>(),
                    )
                })
                .boxed()
        }
        other => unreachable!("arb_map_key_schema never yields {other:?} as a map key"),
    }
}

/// Integer-key generator spanning the primitive's full value range, held
/// in an `i128` so both the unsigned and signed widths fit; the canonical
/// decimal `to_string` is identical to what the corresponding `u64` / `i64`
/// would produce, which is exactly the decoder's emitted key.
fn arb_int_key(p: Primitive) -> BoxedStrategy<i128> {
    match p {
        Primitive::U8 => (0i128..=i128::from(u8::MAX)).boxed(),
        Primitive::U16 => (0i128..=i128::from(u16::MAX)).boxed(),
        Primitive::U32 => (0i128..=i128::from(u32::MAX)).boxed(),
        Primitive::U64 => (0i128..=i128::from(u64::MAX)).boxed(),
        Primitive::I8 => (i128::from(i8::MIN)..=i128::from(i8::MAX)).boxed(),
        Primitive::I16 => (i128::from(i16::MIN)..=i128::from(i16::MAX)).boxed(),
        Primitive::I32 => (i128::from(i32::MIN)..=i128::from(i32::MAX)).boxed(),
        Primitive::I64 => (i128::from(i64::MIN)..=i128::from(i64::MAX)).boxed(),
        other => unreachable!("arb_int_key called with non-integer primitive {other:?}"),
    }
}

/// A `serde_json::Value` strategy that produces exactly the shape
/// `decode_schema` emits for `schema`, so the round-trip is value-equal.
/// Recurses over the (depth-bounded) concrete schema tree.
fn value_for_schema(schema: &SchemaType) -> BoxedStrategy<Value> {
    match schema {
        SchemaType::Unit => Just(Value::Null).boxed(),
        SchemaType::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
        SchemaType::Scalar(p) => arb_scalar_value(*p),
        SchemaType::String => prop::collection::vec(any::<char>(), 0..=12)
            .prop_map(|cs| Value::String(cs.into_iter().collect()))
            .boxed(),
        SchemaType::Bytes => prop::collection::vec(any::<u8>(), 0..=12)
            .prop_map(|bytes| Value::Array(bytes.into_iter().map(Value::from).collect()))
            .boxed(),
        SchemaType::Option(inner) => {
            let inner = value_for_schema(inner);
            prop_oneof![Just(Value::Null), inner].boxed()
        }
        SchemaType::Vec(inner) => {
            let inner = value_for_schema(inner);
            prop::collection::vec(inner, 0..=MAX_WIDTH)
                .prop_map(Value::Array)
                .boxed()
        }
        SchemaType::Array { element, len } => {
            let inner = value_for_schema(element);
            let len = *len as usize;
            prop::collection::vec(inner, len..=len)
                .prop_map(Value::Array)
                .boxed()
        }
        SchemaType::Struct { fields, .. } => {
            let names = fields.iter().map(|f| f.name.to_string()).collect();
            let strats = fields.iter().map(|f| value_for_schema(&f.ty)).collect();
            join_object(names, strats)
        }
        SchemaType::Enum { variants } => {
            let arms: Vec<BoxedStrategy<Value>> = variants.iter().map(value_for_variant).collect();
            Union::new(arms).boxed()
        }
        SchemaType::Map { key, value } => value_for_map(key, value),
        SchemaType::Ref(_) | SchemaType::TypeId(_) => {
            unreachable!("arb_schema never generates Ref / TypeId")
        }
    }
}

/// A generated `(schema, value)` pair where `value` is valid under
/// `schema` and lands in the decoder's canonical shape.
fn arb_schema_and_value() -> impl Strategy<Value = (SchemaType, Value)> {
    arb_schema().prop_flat_map(|schema| {
        let value = value_for_schema(&schema);
        value.prop_map(move |v| (schema.clone(), v))
    })
}

proptest! {
    /// The round-trip law: encoding a schema-valid value and decoding the
    /// bytes back reproduces the value exactly. Default 256 cases.
    #[test]
    fn schema_value_round_trips((schema, value) in arb_schema_and_value()) {
        let bytes = match encode_schema(&value, &schema) {
            Ok(b) => b,
            Err(e) => {
                return Err(TestCaseError::fail(format!(
                    "encode failed: {e}\n  schema: {schema:?}\n  value: {value}"
                )));
            }
        };
        let back = match decode_schema(&bytes, &schema) {
            Ok(v) => v,
            Err(e) => {
                return Err(TestCaseError::fail(format!(
                    "decode failed: {e}\n  schema: {schema:?}\n  value: {value}"
                )));
            }
        };
        prop_assert_eq!(
            &back,
            &value,
            "round-trip mismatch\n  schema: {:?}\n  encoded: {:?}",
            schema,
            bytes
        );
    }
}
