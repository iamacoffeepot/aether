//! Tests for the two schema walks.
//!
//! The translator's oracle is deliberately not a whole-document snapshot. A
//! snapshot fails on every cosmetic change and says nothing about whether the
//! emitted schema *describes the same values the wire accepts*. What is
//! pinned instead is either a computed bound that can drift on its own, or
//! the agreement between our two walks and `aether-codec` — the contract that
//! actually matters and the one a future edit can break invisibly.

use std::borrow::Cow;

use aether_codec::encode_schema;
use aether_data::canonical::kind_id_from_parts;
use aether_data::{EnumVariant, MailboxId, NamedField, Primitive, SchemaCell, SchemaType, Tag, tagged_id};
use serde_json::{Value, json};

use super::translate::{translate, translate_tool_schema};
use super::validate::validate_client_value;
use super::vocabulary::is_tagged_identifier;
use super::{SchemaBudget, SchemaError};

fn field(name: &'static str, ty: SchemaType) -> NamedField {
    NamedField { name: Cow::Borrowed(name), ty }
}

fn structure(fields: Vec<NamedField>) -> SchemaType {
    SchemaType::Struct { fields: Cow::Owned(fields), repr_c: false }
}

fn enumeration(variants: Vec<EnumVariant>) -> SchemaType {
    SchemaType::Enum { variants: Cow::Owned(variants) }
}

fn unit_variant(name: &'static str, discriminant: u32) -> EnumVariant {
    EnumVariant::Unit { name: Cow::Borrowed(name), discriminant }
}

fn cell(schema: SchemaType) -> SchemaCell {
    SchemaCell::owned(schema)
}

fn translated(schema: &SchemaType) -> Value {
    translate(schema, SchemaBudget::default()).expect("schema is admissible")
}

/// Reach into a translated struct schema for one property's subschema.
fn property<'a>(schema: &'a Value, name: &str) -> &'a Value {
    schema.get("properties").and_then(|properties| properties.get(name)).expect("property is present")
}

/// The `U64` bound is the one place the translator can silently lose
/// precision: rendering bounds through `f64` would turn the exact maximum
/// into `1.8446744073709552e19`, which advertises a range the codec then
/// refuses. This fails if the bound stops being an exact JSON integer.
#[test]
fn unsigned_bounds_are_exact_integers() {
    let translated = translated(&SchemaType::Scalar(Primitive::U64));

    assert_eq!(translated["type"], json!("integer"));
    assert_eq!(translated["minimum"], json!(0));
    assert_eq!(translated["maximum"], json!(u64::MAX));
    assert!(translated["maximum"].is_u64(), "the maximum must stay an integer, not a float");
}

/// Signed bounds have the same exactness requirement at the other end.
#[test]
fn signed_bounds_are_exact_integers() {
    let translated = translated(&SchemaType::Scalar(Primitive::I64));

    assert_eq!(translated["minimum"], json!(i64::MIN));
    assert_eq!(translated["maximum"], json!(i64::MAX));
}

/// The `F32` bound exists so a client's own validator refuses a number that
/// would become an infinity in the codec's narrowing cast. A bound of
/// `f64::MAX` on an `F32` field would advertise a range the wire cannot hold.
/// This fails if the float bounds are taken from the wrong width.
#[test]
fn float_bounds_follow_the_declared_width() {
    let single = translated(&SchemaType::Scalar(Primitive::F32));
    let double = translated(&SchemaType::Scalar(Primitive::F64));

    assert_eq!(single["type"], json!("number"));
    assert_eq!(single["maximum"], json!(f64::from(f32::MAX)));
    assert_eq!(double["maximum"], json!(f64::MAX));
    assert!(single["maximum"].as_f64() < double["maximum"].as_f64(), "the single-width bound must be the narrower one");
}

/// Every declared field is required, `Option` included, because the codec
/// requires each named field to be present and spells absence as an explicit
/// null. Dropping optional fields from `required` is the tempting, wrong
/// edit: it reads as correct JSON Schema and produces bodies `encode_schema`
/// then rejects with `MissingField`.
#[test]
fn optional_fields_stay_required_and_admit_null() {
    let translated = translated(&structure(vec![
        field("name", SchemaType::String),
        field("note", SchemaType::Option(cell(SchemaType::String))),
    ]));

    assert_eq!(translated["required"], json!(["name", "note"]));
    assert_eq!(translated["additionalProperties"], json!(false));
    assert_eq!(property(&translated, "note")["anyOf"][1], json!({ "type": "null" }));
}

/// A tool root must be object-shaped, and a `Unit` root means "no arguments"
/// rather than "the argument is null". This fails if the top-level special
/// case is dropped and a no-argument tool starts advertising `type: null`,
/// which no client can send an empty arguments object against.
#[test]
fn a_unit_tool_root_is_the_closed_empty_object() {
    let root = translate_tool_schema(&SchemaType::Unit, SchemaBudget::default()).expect("unit is an admissible root");

    assert_eq!(root["type"], json!("object"));
    assert_eq!(root["properties"], json!({}));
    assert_eq!(root["additionalProperties"], json!(false));
    assert_eq!(root["$schema"], json!(super::JSON_SCHEMA_DIALECT));

    // Nested, the same `Unit` means null.
    assert_eq!(property(&translated(&structure(vec![field("nothing", SchemaType::Unit)])), "nothing")["type"], "null");
}

/// A scalar root cannot carry an object-shaped tool contract, so it is
/// refused at registration rather than advertised and later failing a call.
#[test]
fn a_scalar_tool_root_is_refused() {
    let refusal = translate_tool_schema(&SchemaType::String, SchemaBudget::default());

    assert_eq!(refusal, Err(SchemaError::NonObjectRoot));
}

/// The three externally tagged enum shapes are what `encode_enum_body` reads.
/// A one-field tuple passes its field schema through directly while a
/// two-field tuple becomes a fixed array — conflating them is the plausible
/// bug, and it would make one of the two shapes unencodable.
#[test]
fn enum_variants_translate_to_their_wire_shapes() {
    let translated = translated(&enumeration(vec![
        unit_variant("Idle", 0),
        EnumVariant::Tuple { name: Cow::Borrowed("One"), discriminant: 1, fields: Cow::Owned(vec![SchemaType::Bool]) },
        EnumVariant::Tuple {
            name: Cow::Borrowed("Two"),
            discriminant: 2,
            fields: Cow::Owned(vec![SchemaType::Bool, SchemaType::String]),
        },
        EnumVariant::Struct {
            name: Cow::Borrowed("Named"),
            discriminant: 3,
            fields: Cow::Owned(vec![field("count", SchemaType::Scalar(Primitive::U8))]),
        },
    ]));
    let branches = translated["oneOf"].as_array().expect("a variant set is a oneOf");

    assert_eq!(branches[0], json!({ "type": "string", "const": "Idle" }));
    assert_eq!(property(&branches[1], "One"), &json!({ "type": "boolean" }));

    let pair = property(&branches[2], "Two");
    assert_eq!(pair["type"], json!("array"));
    assert_eq!(pair["minItems"], json!(2));
    assert_eq!(pair["maxItems"], json!(2));
    assert_eq!(pair["items"], json!(false));
    assert_eq!(pair["prefixItems"][1], json!({ "type": "string" }));

    assert_eq!(property(&branches[3], "Named")["required"], json!(["count"]));
}

/// A variant set that admits nothing must say so. An empty `oneOf` is
/// invalid JSON Schema, so the always-failing form is the honest rendering.
#[test]
fn an_empty_enum_translates_to_the_always_failing_schema() {
    assert_eq!(translated(&enumeration(Vec::new())), json!({ "not": {} }));
}

/// A `Bytes` leaf is an array of byte values at this boundary — the canonical
/// form `encode_schema` accepts. This fails if it starts translating to a
/// string, which is what a base64 or `$file` alias would imply and which the
/// address-only decision rules out.
#[test]
fn bytes_translate_to_the_canonical_byte_array() {
    let translated = translated(&SchemaType::Bytes);

    assert_eq!(translated["type"], json!("array"));
    assert_eq!(translated["items"], json!({ "type": "integer", "minimum": 0, "maximum": 255 }));
}

/// A fixed-length array states both bounds rather than repeating the element
/// schema `len` times.
#[test]
fn fixed_arrays_state_both_length_bounds() {
    let translated = translated(&SchemaType::Array { element: cell(SchemaType::Scalar(Primitive::F32)), len: 16 });

    assert_eq!(translated["minItems"], json!(16));
    assert_eq!(translated["maxItems"], json!(16));
    assert_eq!(translated["items"]["type"], json!("number"));
}

/// A string-keyed map needs no key restriction, and a fieldless-enum-keyed
/// map is restricted to exactly the variant names the codec will accept back.
#[test]
fn admissible_map_keys_carry_their_restriction() {
    let open = translated(&SchemaType::Map { key: cell(SchemaType::String), value: cell(SchemaType::Bool) });
    assert_eq!(open["additionalProperties"], json!({ "type": "boolean" }));
    assert!(open.get("propertyNames").is_none(), "a string key needs no restriction");

    let keyed = translated(&SchemaType::Map {
        key: cell(enumeration(vec![unit_variant("Red", 0), unit_variant("Green", 1)])),
        value: cell(SchemaType::Bool),
    });
    assert_eq!(keyed["propertyNames"], json!({ "enum": ["Red", "Green"] }));

    let boolean = translated(&SchemaType::Map { key: cell(SchemaType::Bool), value: cell(SchemaType::Bool) });
    assert_eq!(boolean["propertyNames"], json!({ "enum": ["true", "false"] }));
}

/// The registration-time refusals, as a table.
///
/// Each is a shape `aether-codec` would happily encode (or that a
/// hand-authored `SchemaType` can express) and that this boundary refuses.
/// This fails if a refusal is quietly relaxed — which would put an
/// ambiguous or unrenderable descriptor into a public catalog.
#[test]
fn inadmissible_schemas_are_refused_with_their_reason() {
    let integer_key = SchemaType::Map { key: cell(SchemaType::Scalar(Primitive::U32)), value: cell(SchemaType::Bool) };
    let float_key = SchemaType::Map { key: cell(SchemaType::Scalar(Primitive::F64)), value: cell(SchemaType::Bool) };
    let composite_key =
        SchemaType::Map { key: cell(structure(vec![field("a", SchemaType::Bool)])), value: cell(SchemaType::Bool) };
    let payload_enum_key = SchemaType::Map {
        key: cell(enumeration(vec![EnumVariant::Tuple {
            name: Cow::Borrowed("Wrapped"),
            discriminant: 0,
            fields: Cow::Owned(vec![SchemaType::Bool]),
        }])),
        value: cell(SchemaType::Bool),
    };

    for (label, schema) in [
        ("integer key", integer_key),
        ("float key", float_key),
        ("composite key", composite_key),
        ("enum key with a payload", payload_enum_key),
    ] {
        assert!(
            matches!(translate(&schema, SchemaBudget::default()), Err(SchemaError::UnsupportedMapKey { .. })),
            "{label} should be refused as a map key"
        );
    }

    let unknown_identifier = SchemaType::TypeId(0);
    assert!(matches!(
        translate(&unknown_identifier, SchemaBudget::default()),
        Err(SchemaError::UnknownTypeId { type_id: 0 })
    ));

    let repeated_field = structure(vec![field("x", SchemaType::Bool), field("x", SchemaType::String)]);
    assert!(matches!(
        translate(&repeated_field, SchemaBudget::default()),
        Err(SchemaError::DuplicateStructField { .. })
    ));

    let repeated_name = enumeration(vec![unit_variant("Same", 0), unit_variant("Same", 1)]);
    assert!(matches!(
        translate(&repeated_name, SchemaBudget::default()),
        Err(SchemaError::DuplicateEnumVariant { .. })
    ));

    let repeated_discriminant = enumeration(vec![unit_variant("First", 7), unit_variant("Second", 7)]);
    assert!(matches!(
        translate(&repeated_discriminant, SchemaBudget::default()),
        Err(SchemaError::DuplicateEnumDiscriminant { discriminant: 7 })
    ));
}

/// Build a chain of nested options, iteratively, to a chosen depth.
fn nested_option(depth: usize) -> SchemaType {
    let mut schema = SchemaType::String;
    for _ in 0..depth {
        schema = SchemaType::Option(cell(schema));
    }
    schema
}

/// The budgets must refuse rather than truncate, and refuse *at* the limit
/// rather than somewhere near it. A translator that silently stopped
/// expanding would advertise a schema narrower than the one the codec
/// enforces, which is worse than refusing the registration.
#[test]
fn translation_budgets_bind_exactly() {
    let budget = SchemaBudget { maximum_depth: 8, maximum_nodes: 1_024 };

    // A chain of 7 options over a string is 8 nodes deep: exactly the limit.
    assert!(translate(&nested_option(7), budget).is_ok(), "a tree at the depth limit is admissible");
    assert_eq!(translate(&nested_option(8), budget), Err(SchemaError::DepthExceeded { maximum: 8 }));

    let wide = |count: usize| {
        structure(
            (0..count).map(|index| NamedField { name: Cow::Owned(index.to_string()), ty: SchemaType::Bool }).collect(),
        )
    };
    let nodes = SchemaBudget { maximum_depth: 64, maximum_nodes: 10 };

    // The struct itself is one node, so nine fields exactly fills the budget.
    assert!(translate(&wide(9), nodes).is_ok(), "a tree at the node limit is admissible");
    assert_eq!(translate(&wide(10), nodes), Err(SchemaError::NodesExceeded { maximum: 10 }));
}

/// The pattern the translator advertises and the check the validator applies
/// have to describe the same set, and the truth for both is `aether-data`'s
/// own decoder. This fails if either half drifts from it — a client would
/// then be told a spelling is valid that the boundary rejects, or the
/// reverse.
#[test]
fn the_tagged_identifier_check_matches_the_decoder() {
    // The sample comes from `aether-data` itself rather than a hand-typed
    // string, so it cannot be wrong in the same way the code under test is.
    let minted = kind_id_from_parts("aether.mcp.server.probe", &SchemaType::Unit);
    let canonical = tagged_id::encode(minted).expect("a kind id encodes");
    let candidates = [
        canonical.clone(),
        canonical.to_uppercase(),
        canonical.replace("knd", "KND"),
        canonical.replace("knd", "Knd"),
        canonical.replace('-', "_"),
        format!("{canonical}x"),
        canonical.replacen(|c: char| c.is_ascii_alphanumeric(), "1", 1),
    ];

    for candidate in candidates {
        assert_eq!(
            is_tagged_identifier(&candidate, Tag::Kind),
            tagged_id::decode_with_tag(&candidate, Tag::Kind).is_ok(),
            "grammar disagreed with the decoder on {candidate:?}"
        );
    }
}

/// Values the validator accepts must be values `encode_schema` can carry.
///
/// This is the pairing that matters: the validator is the only thing standing
/// between a client body and the codec, and a validator that admits more than
/// the wire does turns a caller error into an internal one. Running the real
/// encoder as the oracle catches that in whichever direction it drifts.
#[test]
fn accepted_values_encode() {
    let admitted: Vec<(&str, SchemaType, Value)> = vec![
        ("scalar bounds", SchemaType::Scalar(Primitive::U8), json!(255)),
        ("negative scalar", SchemaType::Scalar(Primitive::I16), json!(-32_768)),
        ("float", SchemaType::Scalar(Primitive::F32), json!(1.5)),
        ("string", SchemaType::String, json!("text")),
        ("bytes", SchemaType::Bytes, json!([0, 127, 255])),
        ("some", SchemaType::Option(cell(SchemaType::String)), json!("present")),
        ("none", SchemaType::Option(cell(SchemaType::String)), Value::Null),
        ("vector", SchemaType::Vec(cell(SchemaType::Bool)), json!([true, false])),
        ("array", SchemaType::Array { element: cell(SchemaType::Bool), len: 2 }, json!([true, true])),
        (
            "struct with a null option",
            structure(vec![field("a", SchemaType::Bool), field("b", SchemaType::Option(cell(SchemaType::String)))]),
            json!({ "a": true, "b": null }),
        ),
        ("unit variant", enumeration(vec![unit_variant("Idle", 0)]), json!("Idle")),
        (
            "single-field tuple variant",
            enumeration(vec![EnumVariant::Tuple {
                name: Cow::Borrowed("One"),
                discriminant: 0,
                fields: Cow::Owned(vec![SchemaType::Bool]),
            }]),
            json!({ "One": true }),
        ),
        (
            "multi-field tuple variant",
            enumeration(vec![EnumVariant::Tuple {
                name: Cow::Borrowed("Two"),
                discriminant: 0,
                fields: Cow::Owned(vec![SchemaType::Bool, SchemaType::String]),
            }]),
            json!({ "Two": [true, "x"] }),
        ),
        (
            "struct variant",
            enumeration(vec![EnumVariant::Struct {
                name: Cow::Borrowed("Named"),
                discriminant: 0,
                fields: Cow::Owned(vec![field("count", SchemaType::Scalar(Primitive::U8))]),
            }]),
            json!({ "Named": { "count": 3 } }),
        ),
        (
            "string map",
            SchemaType::Map { key: cell(SchemaType::String), value: cell(SchemaType::Bool) },
            json!({ "a": true }),
        ),
        ("unit", SchemaType::Unit, Value::Null),
    ];

    for (label, schema, value) in admitted {
        validate_client_value(&value, &schema, SchemaBudget::default())
            .unwrap_or_else(|error| panic!("{label}: the validator refused an encodable value: {error}"));
        assert!(encode_schema(&value, &schema).is_ok(), "{label}: the codec refused a value the validator accepted");
    }
}

/// The three places this boundary is deliberately narrower than the codec.
///
/// Each of these is a value `encode_schema` accepts and this boundary must
/// not, because accepting it would produce wire bytes the advertised schema
/// never described. This fails if a well-meaning simplification defers to the
/// codec on any of them.
#[test]
fn the_boundary_refuses_what_the_codec_would_accept() {
    let above_single_width = json!(1.0e300);
    let overflow = SchemaType::Scalar(Primitive::F32);
    assert!(encode_schema(&above_single_width, &overflow).is_ok(), "the codec casts without a finite-range check");
    assert!(
        validate_client_value(&above_single_width, &overflow, SchemaBudget::default()).is_err(),
        "a value that becomes an infinity in the cast must be refused"
    );

    let numeric_identifier = json!(0);
    let identifier = SchemaType::TypeId(MailboxId::TYPE_ID);
    assert!(
        encode_schema(&numeric_identifier, &identifier).is_ok(),
        "the codec accepts the numeric compatibility form"
    );
    assert!(
        validate_client_value(&numeric_identifier, &identifier, SchemaBudget::default()).is_err(),
        "the advertised schema says string, so the numeric form must be refused"
    );

    let not_null = json!("anything");
    assert!(encode_schema(&not_null, &SchemaType::Unit).is_ok(), "the codec discards a unit value unchecked");
    assert!(
        validate_client_value(&not_null, &SchemaType::Unit, SchemaBudget::default()).is_err(),
        "a unit field was advertised as null and must require it"
    );
}

/// Structural refusals name where they happened, so a `-32602` can tell a
/// caller which member to fix without echoing their payload back.
#[test]
fn validation_failures_carry_a_path() {
    let schema = structure(vec![field(
        "items",
        SchemaType::Vec(cell(structure(vec![field("count", SchemaType::Scalar(Primitive::U8))]))),
    )]);
    let value = json!({ "items": [{ "count": 1 }, { "count": 999 }] });

    let failure = validate_client_value(&value, &schema, SchemaBudget::default()).expect_err("999 exceeds a u8");

    assert_eq!(failure.path, "$.items[1].count");
    assert!(failure.reason.contains("0..=255"), "the reason should state the admitted range: {}", failure.reason);
}

/// A missing field and an unexpected field are different caller mistakes and
/// must be reported as such. Reporting only "wrong shape" would leave a
/// caller guessing which of the two they made.
#[test]
fn struct_shape_failures_name_the_offending_field() {
    let schema = structure(vec![field("a", SchemaType::Bool), field("b", SchemaType::Bool)]);

    let missing =
        validate_client_value(&json!({ "a": true }), &schema, SchemaBudget::default()).expect_err("a field is absent");
    assert!(missing.reason.contains("missing field `b`"), "{}", missing.reason);

    let extra = validate_client_value(&json!({ "a": true, "b": true, "c": true }), &schema, SchemaBudget::default())
        .expect_err("a field is undeclared");
    assert!(extra.reason.contains("unexpected field `c`"), "{}", extra.reason);
}

/// The validator's stack holds cursors, not nodes, so a wide payload must not
/// consume nesting depth. This fails if a future edit pushes one frame per
/// element — a change that would look harmless and would make a long array
/// trip the depth limit.
#[test]
fn breadth_does_not_consume_nesting_depth() {
    let schema = SchemaType::Vec(cell(SchemaType::Bool));
    let value = Value::Array(vec![json!(true); 4_096]);
    let shallow = SchemaBudget { maximum_depth: 4, maximum_nodes: 1_000_000 };

    assert!(validate_client_value(&value, &schema, shallow).is_ok(), "a long array is not a deep one");
}

/// The value budget is what keeps a large-but-legal payload from being walked
/// without limit. This fails if the ceiling stops being charged per value.
#[test]
fn the_validation_value_budget_binds() {
    let schema = SchemaType::Vec(cell(SchemaType::Bool));
    let value = Value::Array(vec![json!(true); 64]);
    let tight = SchemaBudget { maximum_depth: 16, maximum_nodes: 8 };

    assert!(validate_client_value(&value, &schema, tight).is_err(), "a payload past the value ceiling must be refused");
}
