//! The egress rewrite of ADR-0238 decisions 3 and 5: turn an in-process
//! payload's tag-1 `Blob` fields into tag-0 inline bytes before it leaves the
//! process.
//!
//! In-process mail writes a `Blob` field as tag 1 and the blob's 32-byte hash,
//! and the envelope carries the store entry that hash names. Nothing outside
//! the process can resolve a hash, so every path out rewrites each such field
//! to tag 0, a `u32` length and the bytes, copied from the attachment whose
//! hash it carries. Every other byte is copied through unchanged.
//!
//! [`inline_blobs`] walks the payload the way `decode_schema` does, over the
//! same `aether_data::wire` layout, in two passes. The first pass checks the
//! layout, finds every tag-1 field and its attachment, and sizes the result, so
//! a value over the limit is refused before a single byte is copied. The
//! second pass copies the runs between the tag-1 fields and splices each
//! attachment's bytes in.
//!
//! [`blob_hashes`] runs the same walk without rewriting: it reports every
//! tag-1 hash a payload carries, for the sender-side resolve that attaches
//! each one's entry before the mail leaves its sender. A schema with no
//! `Blob` anywhere in it is answered from the schema alone, so blob-free kinds
//! never have their payload read.
//!
//! The walk recurses only over the schema tree, never over the payload:
//! sequences are loops, and a schema's depth is capped at
//! [`MAX_SCHEMA_DEPTH`]. A sequence of fixed-size elements (which cannot hold
//! a `Blob`, whose size varies) is stepped over in one bounds check, so a
//! sequence of zero-byte elements cannot spin the walk; every other element
//! consumes at least one byte, which bounds its loop by the payload length.

use std::{error, fmt};

use aether_data::{BlobHash, EnumVariant, Primitive, SchemaType};

use crate::DecodeError;

#[cfg(test)]
mod tests;

/// The deepest schema nesting the walk follows before it refuses the payload
/// as malformed. Real kinds nest a handful of levels; the cap only keeps a
/// pathological descriptor from overflowing the stack.
pub const MAX_SCHEMA_DEPTH: usize = 128;

/// Bytes of a tag-1 field: the tag, then the 32-byte hash.
const HASH_FIELD_BYTES: usize = 1 + 32;

/// Bytes a tag-0 field adds before its content: the tag, then the `u32` length.
const INLINE_HEADER_BYTES: usize = 1 + 4;

/// Why [`inline_blobs`] refused a payload.
#[derive(Debug)]
pub enum InlineError {
    /// The materialized payload would be `size` bytes, over `limit`. Checked
    /// before any bytes are copied. A single attachment longer than a `u32`
    /// length prefix can state is reported with that ceiling as `limit`.
    TooLarge { size: usize, limit: usize },
    /// A tag-1 field carries a hash no attachment has.
    MissingAttachment { hash: BlobHash },
    /// The payload does not match its schema. The error's path is the byte
    /// offset the walk stopped at.
    Malformed(DecodeError),
}

impl fmt::Display for InlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { size, limit } => {
                write!(f, "materialized payload is {size} bytes, over the {limit}-byte limit")
            }
            Self::MissingAttachment { hash } => {
                f.write_str("tag-1 blob field names hash ")?;
                for byte in hash.as_bytes() {
                    write!(f, "{byte:02x}")?;
                }
                f.write_str(", which no attachment carries")
            }
            Self::Malformed(error) => write!(f, "payload does not match its schema: {error}"),
        }
    }
}

impl error::Error for InlineError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Malformed(error) => Some(error),
            Self::TooLarge { .. } | Self::MissingAttachment { .. } => None,
        }
    }
}

/// Rewrite each tag-1 `Blob` field of `payload` (shaped by `schema`) to tag 0
/// by copying in the bytes of the attachment whose hash it carries; tag-0
/// fields and every other byte are copied through.
///
/// A payload with no tag-1 field comes back byte-identical.
///
/// # Errors
///
/// - [`InlineError::TooLarge`] when the result would exceed `limit_bytes`,
///   decided before anything is copied.
/// - [`InlineError::MissingAttachment`] when a tag-1 hash matches no
///   attachment.
/// - [`InlineError::Malformed`] when `payload` does not follow `schema`'s
///   layout: truncated, trailing bytes, an unknown enum discriminant, an
///   option or blob tag out of range, or a schema nested deeper than
///   [`MAX_SCHEMA_DEPTH`].
pub fn inline_blobs(
    schema: &SchemaType,
    payload: &[u8],
    attachments: &[(BlobHash, &[u8])],
    limit_bytes: usize,
) -> Result<Vec<u8>, InlineError> {
    let splices = find_splices(schema, payload, attachments)?;

    let mut size = payload.len();
    for splice in &splices {
        size = size - HASH_FIELD_BYTES + INLINE_HEADER_BYTES + splice.bytes.len();
    }
    if size > limit_bytes {
        return Err(InlineError::TooLarge { size, limit: limit_bytes });
    }

    let mut out = Vec::with_capacity(size);
    let mut copied = 0;
    for splice in &splices {
        out.extend_from_slice(&payload[copied..splice.offset]);
        out.push(0);
        out.extend_from_slice(&splice.len_prefix.to_le_bytes());
        out.extend_from_slice(splice.bytes);
        copied = splice.offset + HASH_FIELD_BYTES;
    }
    out.extend_from_slice(&payload[copied..]);
    Ok(out)
}

/// Every tag-1 hash in `payload` (shaped by `schema`), in field order, one
/// per field, so a hash two fields carry appears twice. A schema with no
/// `Blob` returns empty without reading `payload`.
///
/// # Errors
///
/// [`InlineError::Malformed`], for the layout faults [`inline_blobs`] refuses,
/// only when `schema` holds a `Blob`. It resolves nothing and sizes nothing,
/// so it never returns [`InlineError::TooLarge`] or
/// [`InlineError::MissingAttachment`].
pub fn blob_hashes(schema: &SchemaType, payload: &[u8]) -> Result<Vec<BlobHash>, InlineError> {
    if !holds_blob(schema, 0) {
        return Ok(Vec::new());
    }
    Ok(hash_fields(schema, payload)?.into_iter().map(|field| field.hash).collect())
}

/// One tag-1 field the rewrite replaces: where its tag byte sits in the
/// payload, and the bytes of the attachment its hash names with their `u32`
/// length prefix.
struct Splice<'a> {
    offset: usize,
    bytes: &'a [u8],
    len_prefix: u32,
}

/// The sizing pass: find every tag-1 field and pair it with the attachment
/// its hash names.
fn find_splices<'a>(
    schema: &SchemaType,
    payload: &[u8],
    attachments: &[(BlobHash, &'a [u8])],
) -> Result<Vec<Splice<'a>>, InlineError> {
    hash_fields(schema, payload)?
        .into_iter()
        .map(|HashField { offset, hash }| {
            let bytes = attachments
                .iter()
                .find_map(|(candidate, bytes)| (*candidate == hash).then_some(*bytes))
                .ok_or(InlineError::MissingAttachment { hash })?;
            // A `u32` length prefix cannot state a longer attachment, and any
            // limit a frame can have is far below it.
            let len_prefix = u32::try_from(bytes.len())
                .map_err(|_| InlineError::TooLarge { size: bytes.len(), limit: u32::MAX as usize })?;
            Ok(Splice { offset, bytes, len_prefix })
        })
        .collect()
}

/// One tag-1 field: where its tag byte sits in the payload, and its hash.
struct HashField {
    offset: usize,
    hash: BlobHash,
}

/// Walk `payload` under `schema`, checking its layout and recording every
/// tag-1 field in payload order.
fn hash_fields(schema: &SchemaType, payload: &[u8]) -> Result<Vec<HashField>, InlineError> {
    let mut walk = Walk { payload, pos: 0, fields: Vec::new() };
    match schema {
        // A cast-shaped root is a `#[repr(C)]` image, and a `Blob` cannot sit
        // in one, so there is nothing to find.
        SchemaType::Struct { repr_c: true, .. } => return Ok(Vec::new()),
        _ => walk.value(schema, 0)?,
    }
    if walk.pos != payload.len() {
        return Err(walk.malformed(|path| DecodeError::TrailingBytes { path, remaining: payload.len() - walk.pos }));
    }
    Ok(walk.fields)
}

/// Whether a `Blob` sits anywhere in `schema`, read from the schema tree
/// alone. `depth` is `schema`'s nesting depth; past [`MAX_SCHEMA_DEPTH`] the
/// answer is `true`, which sends the caller to the walk's own depth refusal.
fn holds_blob(schema: &SchemaType, depth: usize) -> bool {
    if depth > MAX_SCHEMA_DEPTH {
        return true;
    }
    match schema {
        SchemaType::Blob => true,
        SchemaType::Option(inner) | SchemaType::Vec(inner) => holds_blob(inner, depth + 1),
        SchemaType::Array { element, .. } => holds_blob(element, depth + 1),
        SchemaType::Struct { fields, .. } => fields.iter().any(|field| holds_blob(&field.ty, depth + 1)),
        SchemaType::Enum { variants } => variants.iter().any(|variant| match variant {
            EnumVariant::Unit { .. } => false,
            EnumVariant::Tuple { fields, .. } => fields.iter().any(|ty| holds_blob(ty, depth + 1)),
            EnumVariant::Struct { fields, .. } => fields.iter().any(|field| holds_blob(&field.ty, depth + 1)),
        }),
        SchemaType::Map { key, value } => holds_blob(key, depth + 1) || holds_blob(value, depth + 1),
        SchemaType::Unit
        | SchemaType::Bool
        | SchemaType::Scalar(_)
        | SchemaType::TypeId(_)
        | SchemaType::String
        | SchemaType::Bytes => false,
    }
}

struct Walk<'p> {
    payload: &'p [u8],
    pos: usize,
    fields: Vec<HashField>,
}

impl Walk<'_> {
    fn value(&mut self, schema: &SchemaType, depth: usize) -> Result<(), InlineError> {
        if depth > MAX_SCHEMA_DEPTH {
            return Err(InlineError::Malformed(DecodeError::UnsupportedSchema(
                "schema nests deeper than the blob rewrite's depth cap",
            )));
        }
        if let Some(width) = fixed_width(schema, depth) {
            return self.skip(width);
        }
        match schema {
            SchemaType::String | SchemaType::Bytes => {
                let len = self.count()?;
                self.skip(len)
            }
            SchemaType::Blob => self.blob(),
            SchemaType::Option(inner) => match self.byte()? {
                0 => Ok(()),
                1 => self.value(inner, depth + 1),
                byte => Err(malformed_at(self.pos - 1, |path| DecodeError::InvalidBool { path, byte })),
            },
            SchemaType::Vec(inner) => {
                let len = self.count()?;
                self.repeat(inner, len, depth)
            }
            SchemaType::Array { element, len } => self.repeat(element, *len as usize, depth),
            SchemaType::Struct { fields, .. } => {
                for field in fields.iter() {
                    self.value(&field.ty, depth + 1)?;
                }
                Ok(())
            }
            SchemaType::Enum { variants } => {
                let at = self.pos;
                let discriminant = u32::from_le_bytes(self.take::<4>()?);
                let variant = variants.iter().find(|v| v.discriminant() == discriminant).ok_or_else(|| {
                    malformed_at(at, |path| DecodeError::UnknownEnumDiscriminant { path, discriminant })
                })?;
                match variant {
                    EnumVariant::Unit { .. } => Ok(()),
                    EnumVariant::Tuple { fields, .. } => {
                        for ty in fields.iter() {
                            self.value(ty, depth + 1)?;
                        }
                        Ok(())
                    }
                    EnumVariant::Struct { fields, .. } => {
                        for field in fields.iter() {
                            self.value(&field.ty, depth + 1)?;
                        }
                        Ok(())
                    }
                }
            }
            SchemaType::Map { key, value } => {
                let len = self.count()?;
                self.guard_count(len)?;
                for _ in 0..len {
                    self.value(key, depth + 1)?;
                    self.value(value, depth + 1)?;
                }
                Ok(())
            }
            // Every other shape has a fixed width and returned above.
            SchemaType::Unit | SchemaType::Bool | SchemaType::Scalar(_) | SchemaType::TypeId(_) => Ok(()),
        }
    }

    /// `len` elements of `element`. A fixed-size element is stepped over in
    /// one bounds check; any other element consumes at least one byte, so a
    /// count past the bytes left is refused before the loop.
    fn repeat(&mut self, element: &SchemaType, len: usize, depth: usize) -> Result<(), InlineError> {
        if let Some(width) = fixed_width(element, depth + 1) {
            let total = width.saturating_mul(len);
            return self.skip(total);
        }
        self.guard_count(len)?;
        for _ in 0..len {
            self.value(element, depth + 1)?;
        }
        Ok(())
    }

    fn blob(&mut self) -> Result<(), InlineError> {
        let at = self.pos;
        match self.byte()? {
            0 => {
                let len = self.count()?;
                self.skip(len)
            }
            1 => {
                let hash = BlobHash::from_bytes(self.take::<32>()?);
                self.fields.push(HashField { offset: at, hash });
                Ok(())
            }
            tag => Err(malformed_at(at, |path| DecodeError::InvalidBlobTag { path, tag })),
        }
    }

    /// Refuse a count of variable-size elements larger than the bytes left:
    /// each one consumes at least a byte.
    fn guard_count(&self, len: usize) -> Result<(), InlineError> {
        let remaining = self.remaining();
        if len > remaining {
            return Err(self.malformed(|path| DecodeError::Truncated { path, needed: len, had: remaining }));
        }
        Ok(())
    }

    fn remaining(&self) -> usize {
        self.payload.len() - self.pos
    }

    fn skip(&mut self, len: usize) -> Result<(), InlineError> {
        let remaining = self.remaining();
        if len > remaining {
            return Err(self.malformed(|path| DecodeError::Truncated { path, needed: len, had: remaining }));
        }
        self.pos += len;
        Ok(())
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], InlineError> {
        let start = self.pos;
        self.skip(N)?;
        let mut out = [0; N];
        out.copy_from_slice(&self.payload[start..self.pos]);
        Ok(out)
    }

    fn byte(&mut self) -> Result<u8, InlineError> {
        self.take::<1>().map(|[byte]| byte)
    }

    /// A `u32` little-endian length or count.
    fn count(&mut self) -> Result<usize, InlineError> {
        Ok(u32::from_le_bytes(self.take::<4>()?) as usize)
    }

    fn malformed(&self, error: impl FnOnce(String) -> DecodeError) -> InlineError {
        malformed_at(self.pos, error)
    }
}

/// A layout fault at payload byte `offset`, which names the error's path.
fn malformed_at(offset: usize, error: impl FnOnce(String) -> DecodeError) -> InlineError {
    InlineError::Malformed(error(format!("byte {offset}")))
}

/// The wire width of `schema` when every value of it has the same width, or
/// `None` when it varies. A `Blob` varies, so a fixed-width shape holds none.
/// `depth` is `schema`'s nesting depth; past [`MAX_SCHEMA_DEPTH`] the answer
/// is `None`, which sends the walk to its own depth refusal.
fn fixed_width(schema: &SchemaType, depth: usize) -> Option<usize> {
    if depth > MAX_SCHEMA_DEPTH {
        return None;
    }
    match schema {
        SchemaType::Unit => Some(0),
        SchemaType::Bool => Some(1),
        SchemaType::Scalar(primitive) => Some(primitive_width(*primitive)),
        SchemaType::TypeId(_) => Some(8),
        SchemaType::Array { element, len } => fixed_width(element, depth + 1)?.checked_mul(*len as usize),
        SchemaType::Struct { fields, .. } => {
            fields.iter().try_fold(0usize, |total, field| total.checked_add(fixed_width(&field.ty, depth + 1)?))
        }
        SchemaType::String
        | SchemaType::Bytes
        | SchemaType::Blob
        | SchemaType::Option(_)
        | SchemaType::Vec(_)
        | SchemaType::Enum { .. }
        | SchemaType::Map { .. } => None,
    }
}

const fn primitive_width(primitive: Primitive) -> usize {
    match primitive {
        Primitive::U8 | Primitive::I8 => 1,
        Primitive::U16 | Primitive::I16 => 2,
        Primitive::U32 | Primitive::I32 | Primitive::F32 => 4,
        Primitive::U64 | Primitive::I64 | Primitive::F64 => 8,
    }
}
