// Wire-encode: the `write_count` slot narrows a `usize` length to `u32`
// after an explicit `<= u32::MAX` assert (a count past the ceiling is an
// encode error, not a silent truncation). The fixed little-endian byte
// layout is the load-bearing contract; `try_into` would obscure it and is
// not available in const context anyway.
#![allow(clippy::cast_possible_truncation)]

//! Const-fn aether-wire primitives shared across the canonical
//! submodules (schema / labels / inputs). Fixed little-endian integer
//! writers, string length + write helpers, `Option<str>` helpers, and
//! the `Cow` narrowing shims that hand out `&[T]` / `&str` from a
//! borrowed `Cow` in const context.
//!
//! The encoding is the ADR-0118 aether wire format (the same layout
//! `aether_data::wire` drives through serde): little-endian fixed-width
//! integers, a `u32` little-endian length prefix on strings, a one-byte
//! option-presence flag. The const-fn writers here must stay byte-for-byte
//! identical to that runtime path — the `*_runtime_matches_const` cross-checks
//! in `canonical::tests` are the guard.
//!
//! Everything here is `pub(super)` — reachable from sibling submodules via
//! `super::primitives::*`. No runtime allocations; no `std`.

use alloc::borrow::Cow;

use crate::schema::{EnumVariant, LabelNode, NamedField, SchemaType, VariantLabel};

// `Cow::Borrowed::deref` isn't const, so `&cow[i]` / `&cow.as_str()`
// can't be called from a const fn. Hand-roll a `match` per concrete
// slice/str type to narrow `Cow<'static, [T]>` to `&[T]` (or
// `Cow<str>` to `&str`). All panic on `Owned` — only the derive-
// emitted `Cow::Borrowed` path is legal at const-eval here. See the
// module-level `#![allow(clippy::ptr_arg)]` in `canonical/mod.rs`
// for why these take `&Cow` rather than `&[T]` / `&str`.

pub(super) const fn cow_named_fields<'a>(c: &'a Cow<'static, [NamedField]>) -> &'a [NamedField] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[NamedField]> not supported in const"),
    }
}

pub(super) const fn cow_enum_variants<'a>(c: &'a Cow<'static, [EnumVariant]>) -> &'a [EnumVariant] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[EnumVariant]> not supported in const"),
    }
}

pub(super) const fn cow_schema_types<'a>(c: &'a Cow<'static, [SchemaType]>) -> &'a [SchemaType] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[SchemaType]> not supported in const"),
    }
}

pub(super) const fn cow_label_nodes<'a>(c: &'a Cow<'static, [LabelNode]>) -> &'a [LabelNode] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[LabelNode]> not supported in const"),
    }
}

pub(super) const fn cow_variant_labels<'a>(
    c: &'a Cow<'static, [VariantLabel]>,
) -> &'a [VariantLabel] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[VariantLabel]> not supported in const"),
    }
}

pub(super) const fn cow_strs<'a>(
    c: &'a Cow<'static, [Cow<'static, str>]>,
) -> &'a [Cow<'static, str>] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[Cow<str>]> not supported in const"),
    }
}

pub(super) const fn cow_str_as_str<'a>(c: &'a Cow<'static, str>) -> &'a str {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<str> not supported in const"),
    }
}

/// Byte width of a fixed little-endian `u32` on the wire — the size of a
/// collection/string count, a sum-type selector (serde's `variant_index`),
/// and a schema discriminant.
pub(super) const U32_WIDTH: usize = 4;

/// Byte width of a fixed little-endian `u64` on the wire — the size of a
/// typed id (`KindId`) and a `SchemaType::TypeId`.
pub(super) const U64_WIDTH: usize = 8;

/// Byte width of a one-byte flag — `bool` and option-presence.
pub(super) const FLAG_WIDTH: usize = 1;

/// Write `val` as four fixed little-endian bytes (the wire encoding of a
/// `u32` count / selector / discriminant), returning the advanced cursor.
pub(super) const fn write_u32_le(val: u32, out: &mut [u8], cursor: usize) -> usize {
    let bytes = val.to_le_bytes();
    let mut i = 0;
    while i < U32_WIDTH {
        out[cursor + i] = bytes[i];
        i += 1;
    }
    cursor + U32_WIDTH
}

/// Write `val` as eight fixed little-endian bytes (the wire encoding of a
/// `u64` typed id / `TypeId`), returning the advanced cursor.
pub(super) const fn write_u64_le(val: u64, out: &mut [u8], cursor: usize) -> usize {
    let bytes = val.to_le_bytes();
    let mut i = 0;
    while i < U64_WIDTH {
        out[cursor + i] = bytes[i];
        i += 1;
    }
    cursor + U64_WIDTH
}

/// Write a wire collection/string count: `val` narrowed to a fixed
/// little-endian `u32`.
///
/// # Panics
/// Panics if `val > u32::MAX` — fail-fast per ADR-0063: callers walk
/// bounded `const` structures whose lengths fit in `u32` by construction,
/// so an overflow indicates a bug in the caller.
pub(super) const fn write_count(val: usize, out: &mut [u8], cursor: usize) -> usize {
    assert!(
        val <= u32::MAX as usize,
        "canonical: count exceeds u32::MAX"
    );
    write_u32_le(val as u32, out, cursor)
}

/// Wire byte length of `s`: a `u32` little-endian length prefix plus the
/// UTF-8 bytes.
pub(super) const fn str_len(s: &str) -> usize {
    U32_WIDTH + s.len()
}

pub(super) const fn write_str(s: &str, out: &mut [u8], cursor: usize) -> usize {
    let bytes = s.as_bytes();
    let mut pos = write_count(bytes.len(), out, cursor);
    let mut i = 0;
    while i < bytes.len() {
        out[pos] = bytes[i];
        pos += 1;
        i += 1;
    }
    pos
}

/// `Option<Cow<'static, str>>` length/write — used by the labels
/// serializer where nominal labels ride as owned/borrowed `Cow`s.
///
/// Takes `&Option<Cow<...>>` rather than `Option<&Cow<...>>` to keep
/// the const-fn call sites simple — `Option::as_ref` is only `const`
/// since Rust 1.83 and the workspace tracks stable, so a per-site
/// allow here avoids forcing toolchain bumps on every label-emitting
/// crate.
#[allow(clippy::ref_option)]
pub(super) const fn option_str_len(s: &Option<Cow<'static, str>>) -> usize {
    match s {
        None => 1,
        Some(inner) => 1 + str_len(cow_str_as_str(inner)),
    }
}

#[allow(clippy::ref_option)]
pub(super) const fn write_option_str(
    s: &Option<Cow<'static, str>>,
    out: &mut [u8],
    cursor: usize,
) -> usize {
    let mut pos = cursor;
    match s {
        None => {
            out[pos] = 0;
            pos += 1;
        }
        Some(inner) => {
            out[pos] = 1;
            pos += 1;
            pos = write_str(cow_str_as_str(inner), out, pos);
        }
    }
    pos
}

/// `Option<&'static str>` length/write — used by the `InputsRecord`
/// encoders where the macro captures doc strings as plain `&str`
/// without a `Cow` wrapper.
pub(super) const fn option_borrowed_str_len(doc: Option<&str>) -> usize {
    match doc {
        None => 1,
        Some(s) => 1 + str_len(s),
    }
}

pub(super) const fn write_option_borrowed_str(
    doc: Option<&str>,
    out: &mut [u8],
    cursor: usize,
) -> usize {
    match doc {
        None => {
            out[cursor] = 0;
            cursor + 1
        }
        Some(s) => {
            out[cursor] = 1;
            write_str(s, out, cursor + 1)
        }
    }
}
