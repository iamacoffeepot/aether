use super::super::{
    EnumVariant, KindDescriptor, KindId, MAX_FORMAT_DEPTH, Primitive, ReplyFormat, SchemaType, kind_id_from_parts,
};
use aether_data::{NamedField, SchemaCell};
use std::collections::HashMap;

const KIND: &str = "test.publish_result";

fn scalar(primitive: Primitive) -> SchemaType {
    SchemaType::Scalar(primitive)
}

fn byte_array(len: u32) -> SchemaType {
    SchemaType::Array { element: SchemaCell::owned(scalar(Primitive::U8)), len }
}

fn field(name: &'static str, ty: SchemaType) -> NamedField {
    NamedField { name: name.into(), ty }
}

/// A `publish_result`-shaped enum: `Committed { head, artifacts }`, the
/// `Conflict` / `Err` siblings, plus a unit and a two-field tuple variant so
/// every refusal rule has a node to fire on.
fn publish_result_schema() -> SchemaType {
    SchemaType::Enum {
        variants: vec![
            EnumVariant::Struct {
                name: "Committed".into(),
                discriminant: 0,
                fields: vec![
                    field("head", scalar(Primitive::U64)),
                    field("artifacts", SchemaType::Vec(SchemaCell::owned(byte_array(32)))),
                ]
                .into(),
            },
            EnumVariant::Struct {
                name: "Conflict".into(),
                discriminant: 1,
                fields: vec![field("actual", scalar(Primitive::U64))].into(),
            },
            EnumVariant::Struct {
                name: "Err".into(),
                discriminant: 2,
                fields: vec![field("message", SchemaType::String)].into(),
            },
            EnumVariant::Unit { name: "Empty".into(), discriminant: 3 },
            EnumVariant::Tuple {
                name: "Moved".into(),
                discriminant: 4,
                fields: vec![scalar(Primitive::U64), byte_array(32)].into(),
            },
        ]
        .into(),
    }
}

fn descriptors() -> HashMap<String, KindDescriptor> {
    HashMap::from([(KIND.to_owned(), KindDescriptor { name: KIND.to_owned(), schema: publish_result_schema() })])
}

fn kind_id() -> KindId {
    KindId(kind_id_from_parts(KIND, &publish_result_schema()))
}

fn parse(mask: &serde_json::Value) -> anyhow::Result<ReplyFormat> {
    ReplyFormat::parse(mask.as_object().expect("test masks are objects"), &descriptors())
}

/// Round-trip a reply through the codec so the walk sees exactly the JSON
/// shape `decode_schema` emits.
fn decoded(value: &serde_json::Value, schema: &SchemaType) -> serde_json::Value {
    aether_codec::decode_schema(&aether_codec::encode_schema(value, schema).expect("fixture encodes"), schema)
        .expect("fixture decodes")
}

fn bytes_of(byte: u8, len: usize) -> serde_json::Value {
    serde_json::Value::Array(vec![serde_json::Value::from(byte); len])
}

/// Catches the walk misaligning with the codec's enum JSON: the masked
/// variant's digests turn to hex, its integer stays a number, and a reply in
/// another variant is untouched.
#[test]
fn a_variant_mask_hexes_its_digests_and_leaves_everything_else() {
    let schema = publish_result_schema();
    let format =
        parse(&serde_json::json!({ KIND: { "Committed": { "artifacts": ["$hex"] } } })).expect("mask is valid");

    let committed = decoded(
        &serde_json::json!({ "Committed": { "head": 7, "artifacts": [bytes_of(0, 32), bytes_of(0xab, 32)] } }),
        &schema,
    );
    let conflict = decoded(&serde_json::json!({ "Conflict": { "actual": 3 } }), &schema);

    assert_eq!(
        format.apply(kind_id(), committed, &schema),
        serde_json::json!({ "Committed": { "head": 7, "artifacts": ["00".repeat(32), "ab".repeat(32)] } }),
    );
    assert_eq!(format.apply(kind_id(), conflict.clone(), &schema), conflict);
}

/// Catches a validator that lets through a mask that cannot apply: each is
/// refused, naming the JSON path and the reason.
#[test]
fn masks_that_cannot_apply_are_refused_with_their_path() {
    let cases = [
        (serde_json::json!({ "test.unknown": "$hex" }), vec!["unknown kind: test.unknown"]),
        (serde_json::json!({ KIND: { "Comitted": { "artifacts": ["$hex"] } } }), vec!["$:", "no variant \"Comitted\""]),
        (serde_json::json!({ KIND: { "Committed": { "artefacts": ["$hex"] } } }), vec!["$.Committed:", "no field"]),
        (serde_json::json!({ KIND: { "Empty": "$hex" } }), vec!["$.Empty:", "unit variant"]),
        (serde_json::json!({ KIND: { "Err": { "message": "$hex" } } }), vec!["$.Err.message:", "$hex", "String"]),
        (
            serde_json::json!({ KIND: { "Committed": { "artifacts": ["$base64"] } } }),
            vec!["$.Committed.artifacts[*]:", "$base64"],
        ),
        (serde_json::json!({ "*": "$hex", KIND: { "Moved": [null, "$hex"] } }), vec!["only key"]),
        (serde_json::json!({ KIND: {} }), vec!["$:", "empty object"]),
        (serde_json::json!({ KIND: { "Moved": ["$hex"] } }), vec!["$.Moved:", "positional"]),
        (serde_json::json!({}), vec!["empty mask"]),
    ];

    for (mask, expected) in cases {
        let error = parse(&mask).expect_err("the mask cannot apply").to_string();
        for part in expected {
            assert!(error.contains(part), "{mask} refused with {error:?}, which should contain {part:?}");
        }
    }
}

/// Catches unbounded recursion on a caller-controlled mask: nesting past
/// the cap is refused instead of walked.
#[test]
fn mask_nesting_past_the_depth_cap_is_refused() {
    let mut schema = scalar(Primitive::U8);
    let mut mask = serde_json::json!("$hex");
    for _ in 0..=MAX_FORMAT_DEPTH {
        schema = SchemaType::Vec(SchemaCell::owned(schema));
        mask = serde_json::json!([mask]);
    }
    let descriptors =
        HashMap::from([("test.deep".to_owned(), KindDescriptor { name: "test.deep".to_owned(), schema })]);

    let error = ReplyFormat::parse(
        serde_json::json!({ "test.deep": mask }).as_object().expect("mask is an object"),
        &descriptors,
    )
    .expect_err("an over-deep mask is refused");
    assert!(error.to_string().contains("deeper than"), "{error}");
}

/// Catches the wildcard over-reaching: it formats `[u8; N]` and `Bytes`
/// leaves, never an integer or a non-byte array.
#[test]
fn the_wildcard_formats_byte_leaves_only() {
    let schema = SchemaType::Struct {
        fields: vec![
            field("digest", byte_array(4)),
            field("blob", SchemaType::Bytes),
            field("count", scalar(Primitive::U32)),
            field("color", SchemaType::Array { element: SchemaCell::owned(scalar(Primitive::F32)), len: 3 }),
        ]
        .into(),
        repr_c: false,
    };
    let format = parse(&serde_json::json!({ "*": "$hex" })).expect("the wildcard is valid");
    let reply = decoded(
        &serde_json::json!({ "digest": [1, 2, 3, 4], "blob": [255], "count": 9, "color": [0.5, 0.25, 1.0] }),
        &schema,
    );

    assert_eq!(
        format.apply(KindId(1), reply, &schema),
        serde_json::json!({ "digest": "01020304", "blob": "ff", "count": 9, "color": [0.5, 0.25, 1.0] }),
    );
}
