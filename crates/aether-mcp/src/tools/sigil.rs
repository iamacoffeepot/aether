//! The closed `$` function table: the one place that says which `$`
//! function applies to which schema leaf, shared by the input embed
//! resolver (`bytes::resolve_bytes_params`) and the reply `format` mask
//! validator (`reply_format::ReplyFormat::parse`), so a function on a leaf
//! type outside its set fails the same way in both directions.
//!
//! | function | applies to | input | output (`format`) |
//! |---|---|---|---|
//! | `$file`, `$base64`, `$text` | `Bytes`, `Blob` | yes | reserved, refused |
//! | `$hex` | `[u8; N]`, `Bytes`, `Blob`, integer scalars | yes | yes |
//!
//! The hex spelling has exactly one form per value: lowercase `[0-9a-f]`,
//! no prefix, two characters per byte. A byte leaf spells its bytes in index
//! order; an integer spells its fixed-width big-endian bit pattern, so a
//! signed value is its two's complement (`i8` -1 is `"ff"`).

use super::render::render_shape;
use super::{Primitive, SchemaType};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use std::fmt::Write as _;
use tokio::fs;

/// One `$` function. Parsed from an embed object's key on input, and from a
/// mask leaf's string on output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sigil {
    File,
    Base64,
    Text,
    Hex,
}

const ALL: [Sigil; 4] = [Sigil::File, Sigil::Base64, Sigil::Text, Sigil::Hex];

impl Sigil {
    /// The function a `$` key names, or `None` for any other text.
    pub(super) fn parse(key: &str) -> Option<Self> {
        ALL.into_iter().find(|sigil| sigil.name() == key)
    }

    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::File => "$file",
            Self::Base64 => "$base64",
            Self::Text => "$text",
            Self::Hex => "$hex",
        }
    }

    /// The function-to-leaf-type table. `schema` is the leaf itself, with
    /// any `Option` layers already removed by the caller.
    pub(super) fn applies_to(self, schema: &SchemaType) -> bool {
        match self {
            Self::File | Self::Base64 | Self::Text => matches!(schema, SchemaType::Bytes | SchemaType::Blob),
            Self::Hex => is_byte_leaf(schema) || integer_width(schema).is_some(),
        }
    }

    /// Whether the function has an output spelling a `format` mask may name.
    /// `$file` / `$base64` / `$text` stay reserved as output functions.
    pub(super) const fn renders(self) -> bool {
        matches!(self, Self::Hex)
    }

    /// Re-encode one decoded reply leaf. A value not in the shape the leaf
    /// decodes to passes through untouched: a reply projection never errors.
    pub(super) fn render(self, value: Value, schema: &SchemaType) -> Value {
        match self {
            Self::Hex if is_byte_leaf(schema) => {
                byte_values(&value).map_or(value, |bytes| Value::String(encode_hex(&bytes)))
            }
            Self::Hex => match schema {
                SchemaType::Scalar(primitive) => int_to_hex(&value, *primitive).map_or(value, Value::String),
                _ => value,
            },
            Self::File | Self::Base64 | Self::Text => value,
        }
    }

    /// Resolve one embed body at `schema` into the canonical JSON
    /// `encode_schema` accepts: a byte-number array for a byte leaf, a JSON
    /// number for an integer leaf. The caller has already checked
    /// [`Self::applies_to`]. `$file` is refused above `max_file_bytes` (the
    /// RPC frame cap) so a blob too large to ride in mail names the
    /// staged-path mechanism instead of being inlined.
    pub(super) async fn resolve_input(
        self,
        body: Value,
        schema: &SchemaType,
        max_file_bytes: usize,
    ) -> anyhow::Result<Value> {
        let name = self.name();
        let text = body
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("{name} at a {} leaf takes a string, got {body}", render_shape(schema)))?;
        let bytes = match self {
            Self::File => {
                let bytes = fs::read(text).await.map_err(|e| anyhow::anyhow!("$file: reading {text:?}: {e}"))?;
                if bytes.len() > max_file_bytes {
                    anyhow::bail!(
                        "$file {text:?} is {} bytes, over the {max_file_bytes}-byte RPC frame cap; a \
                         blob this large must stage as a hub-read path (ADR-0115/0116), not inline \
                         into mail",
                        bytes.len()
                    );
                }
                bytes
            }
            Self::Base64 => STANDARD.decode(text).map_err(|e| anyhow::anyhow!("$base64: invalid base64: {e}"))?,
            Self::Text => text.as_bytes().to_vec(),
            Self::Hex => {
                return hex_input(text, schema)
                    .map_err(|e| anyhow::anyhow!("$hex at a {} leaf: {e}", render_shape(schema)));
            }
        };
        Ok(byte_array(bytes))
    }
}

/// Resolve a `$hex` body at its leaf: a JSON number for an integer, a byte
/// array of exactly `N` for `[u8; N]`, any whole number of bytes otherwise.
fn hex_input(text: &str, schema: &SchemaType) -> anyhow::Result<Value> {
    match schema {
        SchemaType::Scalar(primitive) => int_from_hex(text, *primitive),
        SchemaType::Array { len, .. } => Ok(byte_array(decode_hex(text, Some(usize::try_from(*len)?))?)),
        _ => Ok(byte_array(decode_hex(text, None)?)),
    }
}

fn byte_array(bytes: Vec<u8>) -> Value {
    Value::Array(bytes.into_iter().map(Value::from).collect())
}

/// The functions that apply to `schema`, spelled for an error message.
pub(super) fn applicable_names(schema: &SchemaType) -> String {
    let names: Vec<&str> = ALL.into_iter().filter(|sigil| sigil.applies_to(schema)).map(Sigil::name).collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(" / ")
    }
}

/// A byte-array leaf: `[u8; N]`, `Bytes`, or `Blob`. The `"*"` wildcard
/// covers exactly these.
pub(super) fn is_byte_leaf(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Bytes | SchemaType::Blob => true,
        SchemaType::Array { element, .. } => matches!(**element, SchemaType::Scalar(Primitive::U8)),
        _ => false,
    }
}

/// The byte width of an integer scalar leaf, `None` for every other leaf.
pub(super) fn integer_width(schema: &SchemaType) -> Option<usize> {
    match schema {
        SchemaType::Scalar(Primitive::U8 | Primitive::I8) => Some(1),
        SchemaType::Scalar(Primitive::U16 | Primitive::I16) => Some(2),
        SchemaType::Scalar(Primitive::U32 | Primitive::I32) => Some(4),
        SchemaType::Scalar(Primitive::U64 | Primitive::I64) => Some(8),
        _ => None,
    }
}

/// Lowercase hex, two characters per byte, in index order.
pub(super) fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        // Writing into a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Strict inverse of [`encode_hex`]: lowercase digits only, even length, and
/// exactly `expected_bytes` bytes when given. Uppercase is refused rather than
/// normalized so every value has exactly one spelling.
pub(super) fn decode_hex(text: &str, expected_bytes: Option<usize>) -> anyhow::Result<Vec<u8>> {
    if let Some(bad) = text.chars().find(|c| !matches!(c, '0'..='9' | 'a'..='f')) {
        anyhow::bail!("{bad:?} is not a lowercase hex digit (0-9, a-f; no prefix, no uppercase)");
    }
    if let Some(expected) = expected_bytes
        && text.len() != 2 * expected
    {
        anyhow::bail!("expected exactly {} hex characters for {expected} bytes, got {}", 2 * expected, text.len());
    }
    if !text.len().is_multiple_of(2) {
        anyhow::bail!("an odd number of hex characters ({}) spells no whole byte", text.len());
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| -> anyhow::Result<u8> { Ok(u8::from_str_radix(str::from_utf8(pair)?, 16)?) })
        .collect()
}

/// The fixed-width hex of an integer leaf's JSON number, most significant
/// digit first; a signed value spells its two's-complement bit pattern.
/// `None` when the value is not a number that fits `primitive`.
pub(super) fn int_to_hex(value: &Value, primitive: Primitive) -> Option<String> {
    let unsigned = || value.as_u64();
    let signed = || value.as_i64();
    let bytes: Vec<u8> = match primitive {
        Primitive::U8 => u8::try_from(unsigned()?).ok()?.to_be_bytes().to_vec(),
        Primitive::U16 => u16::try_from(unsigned()?).ok()?.to_be_bytes().to_vec(),
        Primitive::U32 => u32::try_from(unsigned()?).ok()?.to_be_bytes().to_vec(),
        Primitive::U64 => unsigned()?.to_be_bytes().to_vec(),
        Primitive::I8 => i8::try_from(signed()?).ok()?.to_be_bytes().to_vec(),
        Primitive::I16 => i16::try_from(signed()?).ok()?.to_be_bytes().to_vec(),
        Primitive::I32 => i32::try_from(signed()?).ok()?.to_be_bytes().to_vec(),
        Primitive::I64 => signed()?.to_be_bytes().to_vec(),
        Primitive::F32 | Primitive::F64 => return None,
    };
    Some(encode_hex(&bytes))
}

/// Exact inverse of [`int_to_hex`]: exactly two characters per byte of the
/// type, read most significant first, signed types as two's complement.
pub(super) fn int_from_hex(text: &str, primitive: Primitive) -> anyhow::Result<Value> {
    let width = integer_width(&SchemaType::Scalar(primitive)).ok_or_else(|| {
        anyhow::anyhow!("$hex spells integers only, not {}", render_shape(&SchemaType::Scalar(primitive)))
    })?;
    let bytes = decode_hex(text, Some(width))?;
    Ok(match primitive {
        Primitive::U8 => Value::from(u8::from_be_bytes(fixed(&bytes)?)),
        Primitive::U16 => Value::from(u16::from_be_bytes(fixed(&bytes)?)),
        Primitive::U32 => Value::from(u32::from_be_bytes(fixed(&bytes)?)),
        Primitive::U64 => Value::from(u64::from_be_bytes(fixed(&bytes)?)),
        Primitive::I8 => Value::from(i8::from_be_bytes(fixed(&bytes)?)),
        Primitive::I16 => Value::from(i16::from_be_bytes(fixed(&bytes)?)),
        Primitive::I32 => Value::from(i32::from_be_bytes(fixed(&bytes)?)),
        Primitive::I64 => Value::from(i64::from_be_bytes(fixed(&bytes)?)),
        Primitive::F32 | Primitive::F64 => anyhow::bail!("$hex spells integers only"),
    })
}

fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("expected {N} bytes, got {}", bytes.len()))
}

/// A decoded byte leaf: a JSON array of byte numbers, `None` for any other
/// value.
fn byte_values(value: &Value) -> Option<Vec<u8>> {
    value.as_array()?.iter().map(|item| item.as_u64().and_then(|n| u8::try_from(n).ok())).collect()
}
