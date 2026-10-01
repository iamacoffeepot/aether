//! JSON to and from the ADR-0059 storage encoding, driven by a `SchemaType`
//! alone.
//!
//! The walk mirrors the derived `StorageLeaves` / `StorageElement` impls in
//! `aether_data::storage`, so a value encodes to the bytes its derived
//! `Storage` impl writes:
//!
//! - A struct flattens: each field folds its name onto the parent's path
//!   carry, and every leaf is one TLV record tagged by its path and schema.
//! - An enum writes its variant hash at the `__variant` leaf and flattens the
//!   variant's fields under the variant name; a multi-field tuple variant
//!   indexes them. `Option` is the two-variant enum `None` / `Some`.
//! - `Vec`, fixed arrays, and maps are one record each. A container of
//!   positional elements carries their wire bytes under a schema-folded tag;
//!   a container holding a flattened type carries each such element as a
//!   length-framed record stream under the `__elements` tag. The decoder
//!   reads which form a container took from the tag its record sits under.
//! - A leaf body is the leaf's `aether_data::wire` encoding, written and read
//!   by the wire codec's own helpers, so the JSON conventions are the wire
//!   pair's.
//!
//! What the schema cannot say, the walk assumes:
//!
//! - The decoder reads a container's element form from its record tag, so it
//!   decodes either form. The encoder still takes a container element whose
//!   schema is a non-`repr(C)` struct or an enum as `#[derive(Storage)]` (a
//!   tagged element), because JSON cannot say which derive wrote it: it
//!   writes the tagged form even where the stored value was positional.
//! - A validated newtype (`#[storage(validate)]`) has its inner type's
//!   schema, so it encodes to the same bytes, but its invariant is not
//!   checked: only the Rust type holds it.
//! - Read aliases (`#[storage(alias)]`) are not in the schema; a record
//!   written under an old field name is an unbound record here.
//!
//! A `Blob`, a held-reply `Ticket`, and a `repr(C)` struct in a flattened
//! position have no storage form, and are refused.

use aether_data::storage::{
    BYTES_SCHEMA, ELEMENTS_LEAF, MAX_STORAGE_DEPTH, fold_path_segment, terminate_field_hash_runtime,
    variant_hash_runtime,
};
use aether_data::{EnumVariant, NamedField, SchemaType};
use serde_json::Value;

use crate::{DecodeError, EncodeError, MAX_SCHEMA_DEPTH};

mod decode;
mod encode;
#[cfg(test)]
mod tests;

/// Encode `value` against `schema` into the ADR-0059 storage encoding.
///
/// Accepts the JSON [`crate::encode_schema`] accepts for the same schema,
/// and writes the bytes the derived `Storage` impl writes for the same
/// value. A validated newtype's invariant is not checked; see the module
/// docs for what else a schema cannot say.
///
/// # Errors
///
/// [`EncodeError`] for JSON that does not match `schema`, a schema arm with
/// no storage form, or a schema nested past [`MAX_SCHEMA_DEPTH`] or the
/// storage depth cap.
pub fn encode_storage_schema(value: &Value, schema: &SchemaType) -> Result<Vec<u8>, EncodeError> {
    admit(schema, 0).map_err(|refusal| match refusal {
        Refusal::Ticket => EncodeError::ActorReach { field: "$".into() },
        Refusal::Other(reason) => EncodeError::UnsupportedSchema(reason),
    })?;
    encode::encode(value, schema)
}

/// Decode ADR-0059 storage bytes against `schema` into the JSON
/// [`encode_storage_schema`] accepts.
///
/// Strict, like [`crate::decode_schema_strict`]: at most `maximum_values`
/// JSON values are projected, a non-finite float or a repeated map key is
/// an error, and so is any record `schema` does not bind — a schema-only
/// reader has nowhere to keep one, and dropping it would render a partial
/// value as whole.
///
/// # Errors
///
/// [`DecodeError`] for bytes that do not parse or do not match `schema`, a
/// value over `maximum_values`, a schema arm with no storage form, or a
/// schema nested past [`MAX_SCHEMA_DEPTH`] or the storage depth cap.
pub fn decode_storage_schema(bytes: &[u8], schema: &SchemaType, maximum_values: usize) -> Result<Value, DecodeError> {
    admit(schema, 0).map_err(|refusal| match refusal {
        Refusal::Ticket => DecodeError::ActorReach { path: "$".into() },
        Refusal::Other(reason) => DecodeError::UnsupportedSchema(reason),
    })?;
    decode::decode(bytes, schema, maximum_values)
}

/// Why [`admit`] refused a schema.
enum Refusal {
    Ticket,
    Other(&'static str),
}

/// Check the whole schema tree once, before any walk: its depth is capped,
/// so every later recursion over it (the walks, the canonical-bytes hash,
/// the wire leaf codec) is bounded, and the arms with no storage form are
/// refused wherever they sit.
fn admit(schema: &SchemaType, depth: usize) -> Result<(), Refusal> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(Refusal::Other("schema nests deeper than the storage codec's depth cap"));
    }
    match schema {
        SchemaType::Unit
        | SchemaType::Bool
        | SchemaType::Scalar(_)
        | SchemaType::String
        | SchemaType::Bytes
        | SchemaType::TypeId(_) => Ok(()),
        SchemaType::Blob => Err(Refusal::Other("a blob has no storage form")),
        SchemaType::Ticket { .. } => Err(Refusal::Ticket),
        SchemaType::Option(inner) | SchemaType::Vec(inner) => admit(inner, depth + 1),
        SchemaType::Array { element, .. } => admit(element, depth + 1),
        SchemaType::Map { key, value } => {
            admit(key, depth + 1)?;
            admit(value, depth + 1)
        }
        SchemaType::Struct { fields, .. } => fields.iter().try_for_each(|field| admit(&field.ty, depth + 1)),
        SchemaType::Enum { variants } => variants.iter().try_for_each(|variant| match variant {
            EnumVariant::Unit { .. } => Ok(()),
            EnumVariant::Tuple { fields, .. } => fields.iter().try_for_each(|ty| admit(ty, depth + 1)),
            EnumVariant::Struct { fields, .. } => fields.iter().try_for_each(|field| admit(&field.ty, depth + 1)),
        }),
    }
}

/// Whether the walk has gone past the storage depth cap the derived impls
/// enforce (`StorageError::NestingTooDeep`).
fn too_deep(depth: u32) -> bool {
    depth > MAX_STORAGE_DEPTH
}

/// Whether the encoder takes a container element of this schema as tagged (a
/// framed record stream) rather than positional (its wire bytes) — the
/// schema-side reading of `StorageElement::TAGGED`. A schema does not record
/// which derive produced it, so a flattened shape (a non-`repr(C)` struct or
/// an enum) is assumed tagged, and a container is tagged when anything it
/// holds is. The decoder asks this only to find which tags a container may
/// sit under, and takes the positional one when the tagged one is absent.
fn tagged(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Struct { repr_c, .. } => !repr_c,
        SchemaType::Enum { .. } => true,
        SchemaType::Option(inner) | SchemaType::Vec(inner) => tagged(inner),
        SchemaType::Array { element, .. } => tagged(element),
        SchemaType::Map { key, value } => tagged(key) || tagged(value),
        SchemaType::Unit
        | SchemaType::Bool
        | SchemaType::Scalar(_)
        | SchemaType::String
        | SchemaType::Bytes
        | SchemaType::TypeId(_)
        | SchemaType::Blob
        | SchemaType::Ticket { .. } => false,
    }
}

/// The record tag of a container at `carry` the encoder writes, the
/// schema-side `container_hash`.
fn container_hash(carry: u64, depth: u32, schema: &SchemaType) -> u64 {
    if tagged(schema) {
        elements_hash(carry, depth)
    } else {
        positional_hash(carry, schema)
    }
}

/// The tag of a container whose elements are tagged: the `__elements` leaf,
/// holding a bytes body.
fn elements_hash(carry: u64, depth: u32) -> u64 {
    terminate_field_hash_runtime(fold_path_segment(carry, ELEMENTS_LEAF.as_bytes(), depth), &BYTES_SCHEMA)
}

/// The tag of a container whose elements are positional: the container's own
/// schema, folded at its path.
fn positional_hash(carry: u64, schema: &SchemaType) -> u64 {
    terminate_field_hash_runtime(carry, schema)
}

/// The discriminant a variant writes at its `__variant` leaf: its name
/// hashed with its body schema — `Unit` for a unit variant, the field for a
/// one-field tuple, and a struct of the fields otherwise, as the derive
/// builds it.
fn variant_discriminant(variant: &EnumVariant) -> u64 {
    match variant {
        EnumVariant::Unit { name, .. } => variant_hash_runtime(name, &SchemaType::Unit),
        EnumVariant::Tuple { name, fields, .. } if fields.len() == 1 => variant_hash_runtime(name, &fields[0]),
        EnumVariant::Tuple { name, fields, .. } => {
            let fields: Vec<NamedField> = fields
                .iter()
                .enumerate()
                .map(|(index, ty)| NamedField { name: index.to_string().into(), ty: ty.clone() })
                .collect();
            variant_hash_runtime(name, &SchemaType::Struct { fields: fields.into(), repr_c: false })
        }
        EnumVariant::Struct { name, fields, .. } => {
            variant_hash_runtime(name, &SchemaType::Struct { fields: fields.clone(), repr_c: false })
        }
    }
}
