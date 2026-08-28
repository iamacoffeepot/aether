//! The facts the two walks must agree on.
//!
//! Every rule here is consumed twice: once by [`super::translate()`] to
//! *state* a constraint in JSON Schema, once by
//! [`super::validate_client_value()`] to *enforce* it on a client value.
//! Stating them once is what keeps the advertised schema and the accepted
//! input from drifting apart — a divergence a client would
//! experience as a value its own validator accepted being refused, or worse,
//! the reverse.

use aether_data::{EnumVariant, Primitive, SchemaType, Tag, tag_for_type_id};

use super::SchemaError;

/// The value range a scalar admits, in the form both walks need.
///
/// Integers carry exact mathematical bounds in `i128` so one arm can hold
/// both `i64::MIN` and `u64::MAX`. Floats carry a finite magnitude: the
/// boundary rejects a JSON number outside it rather than letting the codec's
/// narrowing cast turn a finite `f64` into an infinity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum ScalarRange {
    Integer { minimum: i128, maximum: i128 },
    Float { magnitude: f64 },
}

/// The exact range of each `Primitive`.
///
/// `f32::MAX` widened to `f64` is exact, so the `F32` bound is the largest
/// finite value that survives the codec's cast; anything larger would become
/// an infinity the advertised schema never described.
pub(super) const fn scalar_range(primitive: Primitive) -> ScalarRange {
    match primitive {
        Primitive::U8 => ScalarRange::Integer { minimum: 0, maximum: u8::MAX as i128 },
        Primitive::U16 => ScalarRange::Integer { minimum: 0, maximum: u16::MAX as i128 },
        Primitive::U32 => ScalarRange::Integer { minimum: 0, maximum: u32::MAX as i128 },
        Primitive::U64 => ScalarRange::Integer { minimum: 0, maximum: u64::MAX as i128 },
        Primitive::I8 => ScalarRange::Integer { minimum: i8::MIN as i128, maximum: i8::MAX as i128 },
        Primitive::I16 => ScalarRange::Integer { minimum: i16::MIN as i128, maximum: i16::MAX as i128 },
        Primitive::I32 => ScalarRange::Integer { minimum: i32::MIN as i128, maximum: i32::MAX as i128 },
        Primitive::I64 => ScalarRange::Integer { minimum: i64::MIN as i128, maximum: i64::MAX as i128 },
        Primitive::F32 => ScalarRange::Float { magnitude: f32::MAX as f64 },
        Primitive::F64 => ScalarRange::Float { magnitude: f64::MAX },
    }
}

/// How a `Map`'s keys render as JSON property names.
///
/// A map registers only when its key type has a faithful *and* client-usable
/// property-name representation. Integer keys have an exact decimal-string
/// mapping in the codec, but no concise schema states canonical spelling plus
/// the full signed or unsigned range, so they are refused here even though
/// `aether-codec` can encode them. Float and composite keys are refused on
/// both sides.
pub(super) enum MapKeyRule<'a> {
    /// Any string is a valid property name; no restriction is emitted.
    AnyString,
    /// The property name must be one of these exact spellings.
    Enumerated(Vec<&'a str>),
}

/// Admit a map key type, or say why it cannot be one.
pub(super) fn map_key_rule(key: &SchemaType) -> Result<MapKeyRule<'_>, SchemaError> {
    match key {
        SchemaType::String => Ok(MapKeyRule::AnyString),
        SchemaType::Bool => Ok(MapKeyRule::Enumerated(vec!["true", "false"])),
        SchemaType::Enum { variants } => variants
            .iter()
            .map(|variant| match variant {
                EnumVariant::Unit { name, .. } => Ok(name.as_ref()),
                _ => Err(SchemaError::UnsupportedMapKey { reason: "enum map key must have only fieldless variants" }),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(MapKeyRule::Enumerated),
        SchemaType::Scalar(Primitive::F32 | Primitive::F64) => {
            Err(SchemaError::UnsupportedMapKey { reason: "float keys have no ordered canonical spelling" })
        }
        SchemaType::Scalar(_) => Err(SchemaError::UnsupportedMapKey {
            reason: "integer keys have no concise schema for canonical spelling and full range",
        }),
        _ => Err(SchemaError::UnsupportedMapKey { reason: "key must be a string, bool, or fieldless enum" }),
    }
}

/// Base32 body alphabet of a tagged identifier: RFC 4648 lowercase, digits
/// `2` through `7`. Decoding is case-insensitive over the body.
const BODY_CLASS: &str = "[A-Za-z2-7]{4}";

/// The regular expression a tagged identifier of `tag` must match.
///
/// It follows the decoder exactly rather than being merely permissive: the
/// three-letter prefix is accepted in all-lowercase or all-uppercase but
/// never mixed, and the twelve body characters accept either case. Canonical
/// output is always lowercase.
pub(super) fn tagged_identifier_pattern(tag: Tag) -> String {
    let prefix = tag.prefix();
    format!("^(?:{prefix}|{})-{BODY_CLASS}-{BODY_CLASS}-{BODY_CLASS}$", prefix.to_uppercase())
}

/// The tag a `SchemaType::TypeId` payload names.
///
/// An unrecognized identifier refuses registration: neither the translator
/// nor the codec can state its JSON semantics, so admitting it would
/// advertise a shape the wire cannot carry.
pub(super) fn tag_for_schema_type_id(type_id: u64) -> Result<Tag, SchemaError> {
    tag_for_type_id(type_id).ok_or(SchemaError::UnknownTypeId { type_id })
}

/// Whether `candidate` is a tagged identifier of `tag`, by the decoder's own
/// grammar.
///
/// This is the enforcement half of [`tagged_identifier_pattern`]. It is
/// written against the same facts rather than by running a regular
/// expression, so the boundary needs no regex engine; the pattern exists to
/// *tell* a client the rule, this function to hold callers to it.
pub(super) fn is_tagged_identifier(candidate: &str, tag: Tag) -> bool {
    let bytes = candidate.as_bytes();
    if bytes.len() != 18 || bytes[3] != b'-' || bytes[8] != b'-' || bytes[13] != b'-' {
        return false;
    }

    let prefix = &candidate[..3];
    if prefix != tag.prefix() && prefix != tag.prefix().to_uppercase() {
        return false;
    }

    [4..8, 9..13, 14..18]
        .into_iter()
        .flat_map(|group| bytes[group].iter())
        .all(|byte| byte.is_ascii_alphabetic() || (b'2'..=b'7').contains(byte))
}
