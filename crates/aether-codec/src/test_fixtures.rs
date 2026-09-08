//! Cross-test `SchemaType` builders shared between `decode` and
//! `encode` test modules. Both module-local copies of `scalar`,
//! `cast_struct`, `structured_struct`, and `pending_ok_err_variants`
//! moved here so adding a new schema test only declares the helper
//! once. Kept in its own `#[cfg(test)]` module so production builds
//! don't pull in the helpers and so the `pub(crate)` visibility
//! doesn't leak into the public API.

#![cfg(test)]

use aether_data::{EnumVariant, NamedField, Primitive, SchemaType};

/// A tag-correct [`MailboxId`](aether_data::MailboxId) for the typed-id
/// arms. The codec keys on the `mbx-` tag bits and the 8-byte little-endian
/// body, never on which mailbox an id names, so the arms take an id with the
/// right tag rather than hashing a namespace they do not route to.
pub fn sample_mailbox_id() -> aether_data::MailboxId {
    aether_data::MailboxId(aether_data::with_tag(aether_data::Tag::Mailbox, 0x0c0d_ec00_1d))
}

/// A `NamedField` holding a single `Scalar(ty)` shape under `name`.
pub fn scalar(name: &str, ty: Primitive) -> NamedField {
    named(name, SchemaType::Scalar(ty))
}

/// Generic `NamedField { name, ty }` builder for the test fixtures
/// that wrap a non-`Scalar` `SchemaType` (arrays, struct nests, refs).
/// `scalar` is the common-case wrapper over this.
pub fn named(name: &str, ty: SchemaType) -> NamedField {
    NamedField { name: name.to_string().into(), ty }
}

/// `Struct { repr_c: true, fields }` — the cast-shape struct builder
/// (`#[repr(C)]` byte layout, `bytemuck`-decodable on the substrate
/// side).
pub fn cast_struct(fields: Vec<NamedField>) -> SchemaType {
    SchemaType::Struct { fields: fields.into(), repr_c: true }
}

/// `Struct { repr_c: false, fields }` — the structured-shape struct
/// builder, for the everything-else wire variant.
pub fn structured_struct(fields: Vec<NamedField>) -> SchemaType {
    SchemaType::Struct { fields: fields.into(), repr_c: false }
}

/// The `Pending / Ok(u64) / Err { reason: String }` variant set
/// every codec test that needs an enum schema reaches for. Kept here
/// as a `Vec<EnumVariant>` rather than a full `SchemaType::Enum` so
/// callers can extend the variants (e.g. with `Pending` + a tuple
/// variant of a different field shape) without going through a
/// builder method.
pub fn pending_ok_err_variants() -> Vec<EnumVariant> {
    vec![
        EnumVariant::Unit { name: "Pending".into(), discriminant: 0 },
        EnumVariant::Tuple {
            name: "Ok".into(),
            discriminant: 1,
            fields: vec![SchemaType::Scalar(Primitive::U64)].into(),
        },
        EnumVariant::Struct {
            name: "Err".into(),
            discriminant: 2,
            fields: vec![NamedField { name: "reason".into(), ty: SchemaType::String }].into(),
        },
    ]
}
