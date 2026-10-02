//! The text each conversation item sends.
//!
//! A message sends its text, a replayed call its verbatim arguments, and a
//! refused output its stored text. A result is rendered: its stored payload
//! decoded with the schema its output cites and serialized as JSON, with
//! every 32-byte digest as lowercase hex. Object keys serialize sorted, so
//! the rendered bytes depend only on the cited payload and schema, and the
//! same input always sends the same request.

use aether_bloomery_kinds::{Detail, Digest, ErasedRef, Refusal};
use aether_bloomery_program::{Async, Env, ToolSchema};
use aether_codec::decode_storage_schema;
use aether_data::{EnumVariant, Primitive, SchemaType};
use serde_json::Value;

use crate::input::{ToolOutput, TurnItem};

/// Most JSON values one rendered result may hold.
pub const RENDER_MAXIMUM_VALUES: usize = 65_536;

/// The text `item` sends, read from the closure the driver injected.
///
/// # Errors
///
/// The read's refusal, or `Refusal::Refused` when a result is not stored
/// under its schema's kind or does not decode with it: the input was built
/// wrong, and nothing is fetched.
pub async fn item(env: &mut Env<Async>, item: &TurnItem) -> Result<String, Refusal> {
    match item {
        TurnItem::Message { text, .. } | TurnItem::CallOutput { output: ToolOutput::Refused(text), .. } => {
            env.read_text(*text).await
        }
        TurnItem::Call(call) => env.read_text(call.arguments()).await,
        TurnItem::CallOutput { output: ToolOutput::Result { schema, result }, .. } => {
            let schema = env.read(*schema).await?;
            let result = of_kind(&schema, *result)?;
            json(&schema, &env.read_payload(result).await?)
        }
    }
}

/// `result`, when it is cited under `schema`'s kind.
fn of_kind(schema: &ToolSchema, result: ErasedRef) -> Result<ErasedRef, Refusal> {
    if result.kind() == schema.kind_id() {
        Ok(result)
    } else {
        Err(refused(format!("a call output's result is not stored as its schema's kind {}", schema.kind_name())))
    }
}

/// `payload` decoded with `schema`, as JSON.
fn json(schema: &ToolSchema, payload: &[u8]) -> Result<String, Refusal> {
    let schema_type = schema.schema();
    let mut value = decode_storage_schema(payload, &schema_type, RENDER_MAXIMUM_VALUES).map_err(|error| {
        refused(format!("a call output's result does not decode as {}: {error}", schema.kind_name()))
    })?;
    hex_digests(&schema_type, &mut value);
    Ok(serde_json::to_string(&value).expect("a decoded JSON value always serializes"))
}

/// Rewrite every `[u8; 32]` position `schema` gives `json` as its digest's
/// lowercase hex string.
///
/// The walk follows the storage decoder's JSON shapes: a struct is an object
/// of its fields, a unit variant is its name, any other variant is
/// `{name: body}`, an `Option` is `null` or its inner value, and a map is an
/// object keyed by rendered keys. It is iterative, with the schema's own
/// depth already capped by the decoder, and it only replaces values, so
/// object keys keep the decoder's sorted order.
fn hex_digests(schema: &SchemaType, json: &mut Value) {
    let mut found = Vec::new();
    let mut pending: Vec<(&SchemaType, &Value, String)> = vec![(schema, &*json, String::new())];
    while let Some((schema, value, pointer)) = pending.pop() {
        if let Some(digest) = digest_at(schema, value) {
            found.push((pointer, digest));
            continue;
        }
        // Children are pushed in reverse so they pop in document order.
        let mut children: Vec<(&SchemaType, &Value, String)> = Vec::new();
        match (schema, value) {
            (SchemaType::Struct { fields, .. }, Value::Object(object)) => {
                for field in fields.iter() {
                    if let Some(child) = object.get(field.name.as_ref()) {
                        children.push((&field.ty, child, child_pointer(&pointer, &field.name)));
                    }
                }
            }
            (SchemaType::Enum { variants }, Value::Object(object)) => {
                let Some((name, body)) = object.iter().next() else {
                    continue;
                };
                let Some(variant) = variants.iter().find(|variant| variant.name() == name) else {
                    continue;
                };
                let pointer = child_pointer(&pointer, name);
                match (variant, body) {
                    (EnumVariant::Tuple { fields, .. }, body) if fields.len() == 1 => {
                        children.push((&fields[0], body, pointer));
                    }
                    (EnumVariant::Tuple { fields, .. }, Value::Array(items)) => {
                        for (index, (ty, item)) in fields.iter().zip(items).enumerate() {
                            children.push((ty, item, child_pointer(&pointer, &index.to_string())));
                        }
                    }
                    (EnumVariant::Struct { fields, .. }, Value::Object(object)) => {
                        for field in fields.iter() {
                            if let Some(child) = object.get(field.name.as_ref()) {
                                children.push((&field.ty, child, child_pointer(&pointer, &field.name)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            (SchemaType::Option(inner), value) if !value.is_null() => children.push((&**inner, value, pointer)),
            (SchemaType::Vec(element) | SchemaType::Array { element, .. }, Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    children.push((&**element, item, child_pointer(&pointer, &index.to_string())));
                }
            }
            (SchemaType::Map { value: element, .. }, Value::Object(object)) => {
                for (key, item) in object {
                    children.push((&**element, item, child_pointer(&pointer, key)));
                }
            }
            _ => {}
        }
        pending.extend(children.into_iter().rev());
    }
    for (pointer, digest) in &found {
        if let Some(slot) = json.pointer_mut(pointer) {
            *slot = Value::String(digest.to_string());
        }
    }
}

/// The digest `value` holds when `schema` is `[u8; 32]` and `value` is 32
/// bytes.
fn digest_at(schema: &SchemaType, value: &Value) -> Option<Digest> {
    let SchemaType::Array { element, len: 32 } = schema else {
        return None;
    };
    if !matches!(**element, SchemaType::Scalar(Primitive::U8)) {
        return None;
    }
    let Value::Array(items) = value else {
        return None;
    };
    let mut bytes = [0; 32];
    for (slot, item) in bytes.iter_mut().zip(items) {
        *slot = u8::try_from(item.as_u64()?).ok()?;
    }
    (items.len() == 32).then(|| Digest::from_bytes(bytes))
}

/// `pointer` extended by one reference token, escaped per RFC 6901.
fn child_pointer(pointer: &str, token: &str) -> String {
    format!("{pointer}/{}", token.replace('~', "~0").replace('/', "~1"))
}

fn refused(reason: String) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}

#[cfg(test)]
mod tests {
    use core::array;

    use aether_bloomery_kinds::{Digest, Ref};
    use aether_bloomery_program::{Edited, NoDetail, ToolSchema};
    use aether_data::{Storage, StorageData};

    use super::json;

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.muse.render.shape")]
    enum Shape {
        Plain,
        Held(Digest),
        Named { tree: Digest, note: String },
    }

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.muse.render.carrier")]
    struct Carrier {
        shape: Shape,
        maybe: Option<Digest>,
        many: Vec<Digest>,
        short: [u8; 16],
        bytes: Vec<u8>,
    }

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes(array::from_fn(|index| seed.wrapping_add(u8::try_from(index).expect("32 fits a byte"))))
    }

    fn render(carrier: Carrier) -> serde_json::Value {
        let payload = Carrier::encode_storage(&StorageData::from_value(carrier)).expect("a carrier encodes");
        let rendered = json(&ToolSchema::of::<Carrier>(), &payload).expect("a carrier renders");
        serde_json::from_str(&rendered).expect("a render is JSON")
    }

    fn carrier(shape: Shape) -> Carrier {
        Carrier { shape, maybe: None, many: Vec::new(), short: [7; 16], bytes: vec![9; 32] }
    }

    #[test]
    fn a_digest_in_a_one_field_variant_renders_as_hex() {
        // Catches a variant walk that skips a tuple variant's body, leaving its digest as 32 numbers.
        let rendered = render(carrier(Shape::Held(digest(1))));
        assert_eq!(rendered["shape"], serde_json::json!({ "Held": digest(1).to_string() }));
    }

    #[test]
    fn a_digest_in_a_struct_variant_renders_as_hex() {
        // Catches a variant walk that skips a struct variant's fields.
        let rendered = render(carrier(Shape::Named { tree: digest(2), note: "n".into() }));
        assert_eq!(rendered["shape"], serde_json::json!({ "Named": { "tree": digest(2).to_string(), "note": "n" } }));
    }

    #[test]
    fn a_digest_in_a_some_option_renders_as_hex() {
        // Catches an option walk that stops at the option, leaving a present digest as numbers.
        let mut value = carrier(Shape::Plain);
        value.maybe = Some(digest(3));
        assert_eq!(render(value)["maybe"], serde_json::json!(digest(3).to_string()));
    }

    #[test]
    fn digests_in_a_vec_render_as_hex() {
        // Catches a sequence walk that skips its elements.
        let mut value = carrier(Shape::Plain);
        value.many = vec![digest(4), digest(5)];
        assert_eq!(render(value)["many"], serde_json::json!([digest(4).to_string(), digest(5).to_string()]));
    }

    #[test]
    fn arrays_that_are_not_digests_stay_arrays() {
        // Catches a shape guess: a 16-byte array or a 32-byte `Vec<u8>` is not a digest and must not become hex.
        let rendered = render(carrier(Shape::Plain));
        assert_eq!(rendered["short"], serde_json::json!(vec![7; 16]));
        assert_eq!(rendered["bytes"], serde_json::json!(vec![9; 32]));
    }

    #[test]
    fn an_edit_tree_and_detail_digest_render_as_hex() {
        // Catches a tree or detail digest rendered as 32 numbers, which spends tokens on every edit and tells the
        // model nothing.
        let (tree, detail) = (digest(0), Ref::<NoDetail>::of_encoded(&NoDetail).expect("detail"));
        let edited = Edited::new(Ref::from_digest(tree), "Edited path.", detail);
        let payload = Edited::encode_storage(&StorageData::from_value(edited)).expect("an edit encodes");

        let rendered = json(&ToolSchema::of::<Edited>(), &payload).expect("an edit renders");
        let rendered: serde_json::Value = serde_json::from_str(&rendered).expect("a render is JSON");

        assert_eq!((&rendered["summary"], &rendered["tree"]), (&"Edited path.".into(), &tree.to_string().into()));
        assert_eq!(rendered["detail"]["digest"], serde_json::json!(detail.digest().to_string()));
    }
}
