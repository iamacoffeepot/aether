//! The validated `format` reply mask of `send_mail` / `send_mail_traced`.
//!
//! The raw argument is a JSON object; [`ReplyFormat::parse`] is the only way
//! to build a [`ReplyFormat`], and it checks the mask against each named
//! kind's schema, so a mask that cannot apply never exists. The mask mirrors
//! the JSON `aether-codec` decodes a reply to (`decode_wire_value` /
//! `decode_enum_body`): a struct is an object over its fields, an enum is a
//! one-key object over its variant name, a one-field tuple variant unwraps to
//! that field, a wider tuple variant is a positional array, and an `Option`
//! is its inner value or `null`. A leaf is a re-encoding only: the mask never
//! renames, drops, or computes a field, so the `replies` projection and the
//! error recognition read the same keys either way.

use super::render::render_shape;
use super::sigil::{Sigil, is_byte_leaf};
use super::{EngineId, EnumVariant, KindDescriptor, KindId, Mcp, SchemaType, kind_id_from_parts};
use aether_data::NamedField;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::mem;

/// Deepest mask nesting [`ReplyFormat::parse`] walks before refusing.
pub(super) const MAX_FORMAT_DEPTH: usize = 64;

/// The wildcard key: `{"*": "$hex"}` formats every byte-array leaf in every
/// reply.
const WILDCARD: &str = "*";

/// A reply mask that passed validation against the kinds it names. Its mask
/// is private, so [`ReplyFormat::parse`] is the only way to build one.
#[derive(Debug)]
pub(super) struct ReplyFormat(Mask);

#[derive(Debug)]
enum Mask {
    /// Per-kind masks, keyed by the `KindId` of the exact descriptor each was
    /// validated against, so a reply of another shape is never walked with
    /// one.
    Kinds(HashMap<KindId, FormatNode>),
    /// `"*": "$fn"`: every byte-array leaf (`[u8; N]`, `Bytes`, `Blob`) in
    /// every reply. Integers are left alone.
    Every(Sigil),
}

/// One validated mask node, aligned with the schema node it was checked
/// against (after removing any `Option` layers).
#[derive(Debug)]
enum FormatNode {
    /// A leaf the function applies to.
    Leaf(Sigil),
    /// Struct fields, or a struct variant's fields.
    Fields(Vec<(Box<str>, Self)>),
    /// Enum variants that carry a payload, each over that payload.
    Variants(Vec<(Box<str>, Self)>),
    /// Every element of a `Vec` / array, every value of a `Map`.
    Each(Box<Self>),
    /// A tuple variant with two or more fields, position by position.
    Positions(Vec<Option<Self>>),
}

impl ReplyFormat {
    /// Validate `raw` against `descriptors` (kind name → descriptor, as the
    /// target engine resolves them). Every rule the grammar states is
    /// enforced here; an error names the kind, the JSON path inside its
    /// mask, and the function and leaf type where they apply.
    pub(super) fn parse(
        raw: &Map<String, Value>,
        descriptors: &HashMap<String, KindDescriptor>,
    ) -> anyhow::Result<Self> {
        if raw.is_empty() {
            anyhow::bail!("format: an empty mask; name at least one reply kind, or \"*\"");
        }

        if let Some(value) = raw.get(WILDCARD) {
            if raw.len() != 1 {
                anyhow::bail!("format: \"*\" must be the only key; a mask is either the wildcard or per-kind masks");
            }
            let sigil = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("format: \"*\" takes a function name such as \"$hex\", got {value}"))
                .and_then(|name| output_sigil(name).map_err(|reason| anyhow::anyhow!("format: \"*\": {reason}")))?;
            return Ok(Self(Mask::Every(sigil)));
        }

        let mut kinds = HashMap::with_capacity(raw.len());
        for (name, mask) in raw {
            let descriptor = descriptors.get(name).ok_or_else(|| anyhow::anyhow!("format: unknown kind: {name}"))?;
            let node = parse_node(mask, &descriptor.schema, "$", 0)
                .map_err(|refusal| anyhow::anyhow!("format: {name}: {}: {}", refusal.path, refusal.reason))?;
            kinds.insert(KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema)), node);
        }
        Ok(Self(Mask::Kinds(kinds)))
    }

    /// Format one decoded reply of kind `kind` whose schema is `schema`. A
    /// reply whose kind the mask does not name, with no wildcard, is returned
    /// unchanged.
    pub(super) fn apply(&self, kind: KindId, value: Value, schema: &SchemaType) -> Value {
        match &self.0 {
            Mask::Kinds(kinds) => match kinds.get(&kind) {
                Some(node) => apply_node(node, value, schema),
                None => value,
            },
            Mask::Every(sigil) => apply_every(*sigil, value, schema),
        }
    }
}

/// Resolve every kind `raw` names on `engine` — the same resolve-then-refresh
/// path `send_mail` params encode through — and validate the mask against
/// those descriptors.
pub(super) async fn resolve_reply_format(
    mcp: &Mcp,
    engine: EngineId,
    raw: &Map<String, Value>,
) -> anyhow::Result<ReplyFormat> {
    let mut descriptors = HashMap::with_capacity(raw.len());
    for name in raw.keys().filter(|name| name.as_str() != WILDCARD) {
        let descriptor =
            mcp.lookup_descriptor(engine, name).await.map_err(|_| anyhow::anyhow!("format: unknown kind: {name}"))?;
        descriptors.insert(name.clone(), descriptor);
    }
    ReplyFormat::parse(raw, &descriptors)
}

/// Why one mask node was refused, and where.
struct Refusal {
    path: String,
    reason: String,
}

fn refuse(path: &str, reason: impl Into<String>) -> Refusal {
    Refusal { path: path.to_owned(), reason: reason.into() }
}

/// A function a mask leaf may name: a known `$` function with an output
/// spelling.
fn output_sigil(name: &str) -> Result<Sigil, String> {
    let sigil = Sigil::parse(name).ok_or_else(|| format!("unknown function {name:?} (output functions: $hex)"))?;
    if sigil.renders() {
        Ok(sigil)
    } else {
        Err(format!("{name} is reserved as an input function and has no output spelling (output functions: $hex)"))
    }
}

/// Remove the `Option` layers the codec renders transparently.
fn strip_options(mut schema: &SchemaType) -> &SchemaType {
    while let SchemaType::Option(inner) = schema {
        schema = inner;
    }
    schema
}

fn parse_node(mask: &Value, schema: &SchemaType, path: &str, depth: usize) -> Result<FormatNode, Refusal> {
    if depth > MAX_FORMAT_DEPTH {
        return Err(refuse(path, format!("the mask nests deeper than {MAX_FORMAT_DEPTH} levels")));
    }
    let schema = strip_options(schema);
    let shape = || render_shape(schema);

    match mask {
        Value::String(name) => {
            let sigil = output_sigil(name).map_err(|reason| refuse(path, reason))?;
            if sigil.applies_to(schema) {
                Ok(FormatNode::Leaf(sigil))
            } else {
                Err(refuse(path, format!("{name} does not apply to a {} leaf", shape())))
            }
        }
        Value::Object(keys) if keys.is_empty() => Err(refuse(path, "an empty object formats nothing")),
        Value::Object(keys) => match schema {
            SchemaType::Struct { fields, .. } => parse_fields(keys, fields, path, depth).map(FormatNode::Fields),
            SchemaType::Enum { variants } => parse_variants(keys, variants, path, depth).map(FormatNode::Variants),
            _ => Err(refuse(path, format!("an object mask needs a struct or an enum, and the leaf is {}", shape()))),
        },
        Value::Array(items) => match schema {
            SchemaType::Vec(inner)
            | SchemaType::Array { element: inner, .. }
            | SchemaType::Map { value: inner, .. } => match items.as_slice() {
                [item] => Ok(FormatNode::Each(Box::new(parse_node(item, inner, &format!("{path}[*]"), depth + 1)?))),
                _ => Err(refuse(
                    path,
                    format!("an array mask over {} holds exactly one entry, got {}", shape(), items.len()),
                )),
            },
            _ => {
                Err(refuse(path, format!("an array mask needs a Vec, an array, or a Map, and the leaf is {}", shape())))
            }
        },
        Value::Null | Value::Bool(_) | Value::Number(_) => {
            Err(refuse(path, format!("a mask node is a function name, an object, or an array, got {mask}")))
        }
    }
}

fn parse_fields(
    keys: &Map<String, Value>,
    fields: &[NamedField],
    path: &str,
    depth: usize,
) -> Result<Vec<(Box<str>, FormatNode)>, Refusal> {
    if keys.is_empty() {
        return Err(refuse(path, "an empty object formats nothing"));
    }
    keys.iter()
        .map(|(key, mask)| {
            let field = fields.iter().find(|field| field.name == key.as_str()).ok_or_else(|| {
                let names: Vec<&str> = fields.iter().map(|field| field.name.as_ref()).collect();
                refuse(path, format!("no field {key:?} (fields: {})", names.join(", ")))
            })?;
            Ok((key.as_str().into(), parse_node(mask, &field.ty, &format!("{path}.{key}"), depth + 1)?))
        })
        .collect()
}

fn parse_variants(
    keys: &Map<String, Value>,
    variants: &[EnumVariant],
    path: &str,
    depth: usize,
) -> Result<Vec<(Box<str>, FormatNode)>, Refusal> {
    keys.iter()
        .map(|(key, mask)| {
            let variant_path = format!("{path}.{key}");
            let variant = variants.iter().find(|variant| variant.name() == key.as_str()).ok_or_else(|| {
                let names: Vec<&str> = variants.iter().map(EnumVariant::name).collect();
                refuse(path, format!("no variant {key:?} (variants: {})", names.join(", ")))
            })?;
            let node = match variant {
                EnumVariant::Unit { .. } => {
                    return Err(refuse(&variant_path, format!("unit variant {key:?} carries no payload to format")));
                }
                EnumVariant::Tuple { fields, .. } => match fields.as_ref() {
                    [] => {
                        return Err(refuse(&variant_path, format!("variant {key:?} carries no payload to format")));
                    }
                    [single] => parse_node(mask, single, &variant_path, depth + 1)?,
                    _ => FormatNode::Positions(parse_positions(mask, fields, &variant_path, depth + 1)?),
                },
                EnumVariant::Struct { fields, .. } => match mask {
                    Value::Object(inner) => FormatNode::Fields(parse_fields(inner, fields, &variant_path, depth + 1)?),
                    _ => {
                        return Err(refuse(
                            &variant_path,
                            format!("struct variant {key:?} takes an object over its fields, got {mask}"),
                        ));
                    }
                },
            };
            Ok((key.as_str().into(), node))
        })
        .collect()
}

fn parse_positions(
    mask: &Value,
    fields: &[SchemaType],
    path: &str,
    depth: usize,
) -> Result<Vec<Option<FormatNode>>, Refusal> {
    let items = match mask {
        Value::Array(items) if items.len() == fields.len() => items,
        _ => {
            return Err(refuse(
                path,
                format!(
                    "a {}-field tuple variant takes a positional array of {} entries, got {mask}",
                    fields.len(),
                    fields.len()
                ),
            ));
        }
    };
    let positions = items
        .iter()
        .zip(fields)
        .enumerate()
        .map(|(index, (item, field))| match item {
            Value::Null => Ok(None),
            _ => parse_node(item, field, &format!("{path}[{index}]"), depth).map(Some),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if positions.iter().all(Option::is_none) {
        return Err(refuse(path, "a positional array of only null formats nothing"));
    }
    Ok(positions)
}

/// Walk `value` with a node validated against `schema`. The node and schema
/// agree by construction (the mask is keyed by the exact kind id it was
/// validated against); a value in any other shape passes through untouched.
fn apply_node(node: &FormatNode, value: Value, schema: &SchemaType) -> Value {
    if value.is_null() {
        return value;
    }
    let schema = strip_options(schema);

    match (node, schema) {
        (FormatNode::Leaf(sigil), _) => sigil.render(value, schema),
        (FormatNode::Fields(mask), SchemaType::Struct { fields, .. }) => apply_fields(mask, value, fields),
        (FormatNode::Variants(mask), SchemaType::Enum { variants }) => apply_variants(mask, value, variants),
        (
            FormatNode::Each(inner),
            SchemaType::Vec(element) | SchemaType::Array { element, .. } | SchemaType::Map { value: element, .. },
        ) => match value {
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| apply_node(inner, item, element)).collect())
            }
            Value::Object(mut map) => {
                for slot in map.values_mut() {
                    *slot = apply_node(inner, mem::take(slot), element);
                }
                Value::Object(map)
            }
            other => other,
        },
        _ => value,
    }
}

fn apply_fields(mask: &[(Box<str>, FormatNode)], value: Value, fields: &[NamedField]) -> Value {
    let Value::Object(mut map) = value else {
        return value;
    };
    for (name, node) in mask {
        if let (Some(slot), Some(field)) =
            (map.get_mut(name.as_ref()), fields.iter().find(|field| field.name == name.as_ref()))
        {
            *slot = apply_node(node, mem::take(slot), &field.ty);
        }
    }
    Value::Object(map)
}

fn apply_variants(mask: &[(Box<str>, FormatNode)], value: Value, variants: &[EnumVariant]) -> Value {
    let Value::Object(mut map) = value else {
        return value;
    };
    let Some((tag, payload)) = map.iter_mut().next() else {
        return Value::Object(map);
    };
    let (Some((_, node)), Some(variant)) = (
        mask.iter().find(|(name, _)| name.as_ref() == tag.as_str()),
        variants.iter().find(|variant| variant.name() == tag.as_str()),
    ) else {
        return Value::Object(map);
    };

    let taken = mem::take(payload);
    *payload = match (variant, node) {
        (EnumVariant::Tuple { fields, .. }, FormatNode::Positions(positions)) => match taken {
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .zip(fields.iter().zip(positions))
                    .map(|(item, (field, position))| match position {
                        Some(node) => apply_node(node, item, field),
                        None => item,
                    })
                    .collect(),
            ),
            other => other,
        },
        (EnumVariant::Tuple { fields, .. }, _) => match fields.as_ref() {
            [single] => apply_node(node, taken, single),
            _ => taken,
        },
        (EnumVariant::Struct { fields, .. }, FormatNode::Fields(mask)) => apply_fields(mask, taken, fields),
        _ => taken,
    };
    Value::Object(map)
}

/// The wildcard walk: render every byte-array leaf with `sigil`, descending
/// every composite the codec emits. Depth is bounded by the compile-time
/// kind schema, as in `render_bytes_reply`.
fn apply_every(sigil: Sigil, value: Value, schema: &SchemaType) -> Value {
    if is_byte_leaf(schema) {
        return sigil.render(value, schema);
    }
    match (schema, value) {
        (SchemaType::Option(inner), value) if !value.is_null() => apply_every(sigil, value, inner),
        (SchemaType::Vec(inner) | SchemaType::Array { element: inner, .. }, Value::Array(items)) => {
            Value::Array(items.into_iter().map(|item| apply_every(sigil, item, inner)).collect())
        }
        (SchemaType::Map { value: inner, .. }, Value::Object(mut map)) => {
            for slot in map.values_mut() {
                *slot = apply_every(sigil, mem::take(slot), inner);
            }
            Value::Object(map)
        }
        (SchemaType::Struct { fields, .. }, Value::Object(map)) => every_field(sigil, map, fields),
        (SchemaType::Enum { variants }, Value::Object(mut map)) if map.len() == 1 => {
            if let Some((tag, payload)) = map.iter_mut().next()
                && let Some(variant) = variants.iter().find(|variant| variant.name() == tag.as_str())
            {
                let taken = mem::take(payload);
                *payload = match variant {
                    EnumVariant::Unit { .. } => taken,
                    EnumVariant::Tuple { fields, .. } => match (fields.as_ref(), taken) {
                        ([single], taken) => apply_every(sigil, taken, single),
                        (fields, Value::Array(items)) => Value::Array(
                            items
                                .into_iter()
                                .zip(fields)
                                .map(|(item, field)| apply_every(sigil, item, field))
                                .collect(),
                        ),
                        (_, other) => other,
                    },
                    EnumVariant::Struct { fields, .. } => match taken {
                        Value::Object(inner) => every_field(sigil, inner, fields),
                        other => other,
                    },
                };
            }
            Value::Object(map)
        }
        (_, value) => value,
    }
}

fn every_field(sigil: Sigil, mut map: Map<String, Value>, fields: &[NamedField]) -> Value {
    for field in fields {
        if let Some(slot) = map.get_mut(field.name.as_ref()) {
            *slot = apply_every(sigil, mem::take(slot), &field.ty);
        }
    }
    Value::Object(map)
}
