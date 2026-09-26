#[allow(clippy::wildcard_imports)]
use super::super::*;
use aether_data::{NamedField, SchemaCell};

const NO_CAP: usize = usize::MAX;

/// The 32 bytes `0x00..=0x1f`, spelled in index order.
const DIGEST_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

fn digest_schema() -> SchemaType {
    SchemaType::Array { element: SchemaCell::owned(SchemaType::Scalar(Primitive::U8)), len: 32 }
}

/// `{ source: [u8; 32] }` — the shape of a program input citing a tree.
fn source_struct_schema() -> SchemaType {
    SchemaType::Struct { fields: vec![NamedField { name: "source".into(), ty: digest_schema() }].into(), repr_c: false }
}

fn digest_literal() -> serde_json::Value {
    serde_json::Value::Array((0u8..32).map(serde_json::Value::from).collect())
}

async fn resolve(value: serde_json::Value, schema: &SchemaType) -> anyhow::Result<serde_json::Value> {
    resolve_bytes_params(value, schema, NO_CAP).await
}

/// Catches a byte-order or length slip: `$hex` at a `[u8; 32]` field must
/// encode to exactly the wire bytes the literal 32-number array does.
#[tokio::test]
async fn hex_digest_encodes_to_the_same_bytes_as_its_literal_array() {
    let schema = source_struct_schema();

    let embedded =
        resolve(serde_json::json!({ "source": { "$hex": DIGEST_HEX } }), &schema).await.expect("$hex resolves");
    let literal = serde_json::json!({ "source": digest_literal() });

    assert_eq!(
        aether_codec::encode_schema(&embedded, &schema).expect("embedded digest encodes"),
        aether_codec::encode_schema(&literal, &schema).expect("literal digest encodes"),
    );
}

/// Catches a lenient parser admitting a second spelling of one value.
#[tokio::test]
async fn non_canonical_hex_spellings_are_refused() {
    let schema = digest_schema();
    let spellings = [
        DIGEST_HEX.to_uppercase(),
        DIGEST_HEX[..63].to_owned(),
        format!("{DIGEST_HEX}0"),
        format!("0x{}", &DIGEST_HEX[2..]),
        format!("zz{}", &DIGEST_HEX[2..]),
    ];

    for spelling in spellings {
        let refused = resolve(serde_json::json!({ "$hex": spelling }), &schema).await;
        assert!(refused.is_err(), "{spelling:?} must be refused, got {refused:?}");
    }
}

/// Catches the function table not being consulted: a function outside its
/// leaf set is refused naming the function and the leaf type.
#[tokio::test]
async fn functions_outside_their_leaf_set_are_refused_by_name() {
    let cases = [
        (serde_json::json!({ "$base64": "AAEC" }), digest_schema(), ["$base64", "[u8; 32]"]),
        (serde_json::json!({ "$hex": "00" }), SchemaType::String, ["$hex", "String"]),
        (serde_json::json!({ "$hex": "00000000" }), SchemaType::Scalar(Primitive::F32), ["$hex", "f32"]),
    ];

    for (value, schema, names) in cases {
        let error = resolve(value, &schema).await.expect_err("the function does not apply").to_string();
        for name in names {
            assert!(error.contains(name), "{error:?} should name {name:?}");
        }
    }
}

/// Catches an endianness or sign slip: integers read most significant digit
/// first at the type's fixed width, signed types as two's complement.
#[tokio::test]
async fn integer_leaves_read_fixed_width_most_significant_first() {
    let unsigned = SchemaType::Scalar(Primitive::U32);
    let signed = SchemaType::Scalar(Primitive::I8);

    assert_eq!(resolve(serde_json::json!({ "$hex": "0000012c" }), &unsigned).await.expect("u32 resolves"), 300);
    assert_eq!(resolve(serde_json::json!({ "$hex": "ff" }), &signed).await.expect("i8 resolves"), -1);
    assert!(resolve(serde_json::json!({ "$hex": "012c" }), &unsigned).await.is_err(), "a short u32 is refused");
}

/// Catches `$hex` at a variable-length byte leaf enforcing a width it has
/// none of, or accepting half a byte.
#[tokio::test]
async fn hex_at_a_bytes_leaf_takes_any_whole_number_of_bytes() {
    assert_eq!(
        resolve(serde_json::json!({ "$hex": "6869" }), &SchemaType::Bytes).await.expect("even hex resolves"),
        serde_json::json!([104, 105]),
    );
    assert_eq!(
        resolve(serde_json::json!({ "$hex": "" }), &SchemaType::Bytes).await.expect("empty hex resolves"),
        serde_json::json!([]),
    );
    assert!(resolve(serde_json::json!({ "$hex": "686" }), &SchemaType::Bytes).await.is_err(), "odd length is refused");
}

/// Catches input and output spellings that disagree: a leaf rendered with
/// `$hex` and fed back as a `$hex` embed encodes to the original bytes.
#[tokio::test]
async fn a_rendered_hex_leaf_feeds_back_as_the_same_bytes() {
    let cases = [(digest_schema(), digest_literal()), (SchemaType::Scalar(Primitive::I16), serde_json::json!(-2))];

    for (schema, literal) in cases {
        let original = aether_codec::encode_schema(&literal, &schema).expect("literal encodes");
        let decoded = aether_codec::decode_schema(&original, &schema).expect("original decodes");
        let rendered = Sigil::Hex.render(decoded, &schema);
        assert!(rendered.is_string(), "the leaf renders as a hex string: {rendered}");

        let resolved = resolve(serde_json::json!({ "$hex": rendered }), &schema).await.expect("rendered hex resolves");
        assert_eq!(aether_codec::encode_schema(&resolved, &schema).expect("resolved encodes"), original);
    }
}

/// Catches the strict decoder and the encoder disagreeing on case or order.
#[test]
fn hex_spelling_is_lowercase_in_index_order() {
    let bytes: Vec<u8> = (0u8..32).collect();

    assert_eq!(encode_hex(&bytes), DIGEST_HEX);
    assert_eq!(decode_hex(DIGEST_HEX, Some(32)).expect("canonical spelling decodes"), bytes);
}
