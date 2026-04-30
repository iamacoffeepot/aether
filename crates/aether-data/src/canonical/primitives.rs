//! Const-fn postcard primitives shared across the canonical
//! submodules (schema / labels / inputs). Varint encoders, string
//! length + write helpers, `Option<str>` helpers, and the `Cow`
//! narrowing shims that hand out `&[T]` / `&str` from a borrowed
//! `Cow` in const context.
//!
//! Everything here is either `pub` (re-exported at `canonical::*`)
//! or `pub(super)` (reachable from sibling submodules via
//! `super::primitives::*`). No runtime allocations; no `std`.

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

/// Byte count of `val` in postcard's unsigned-varint encoding
/// (7 bits per byte, MSB set until the last byte).
pub const fn varint_u32_len(val: u32) -> usize {
    if val < (1 << 7) {
        1
    } else if val < (1 << 14) {
        2
    } else if val < (1 << 21) {
        3
    } else if val < (1 << 28) {
        4
    } else {
        5
    }
}

pub const fn varint_usize_len(val: usize) -> usize {
    if val > u32::MAX as usize {
        panic!("varint_usize_len: value exceeds u32::MAX");
    }
    varint_u32_len(val as u32)
}

/// Byte count of `val` in postcard's unsigned-varint encoding for u64.
/// Extends `varint_u32_len` to the full u64 range needed by `Kind::ID`.
pub const fn varint_u64_len(val: u64) -> usize {
    let mut bytes = 1usize;
    let mut v = val >> 7;
    while v != 0 {
        bytes += 1;
        v >>= 7;
    }
    bytes
}

pub(super) const fn write_varint_u32(mut val: u32, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    while val >= 0x80 {
        out[pos] = ((val & 0x7F) as u8) | 0x80;
        val >>= 7;
        pos += 1;
    }
    out[pos] = val as u8;
    pos + 1
}

pub(super) const fn write_varint_usize(val: usize, out: &mut [u8], cursor: usize) -> usize {
    if val > u32::MAX as usize {
        panic!("write_varint_usize: value exceeds u32::MAX");
    }
    write_varint_u32(val as u32, out, cursor)
}

pub(super) const fn write_varint_u64(mut val: u64, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    while val >= 0x80 {
        out[pos] = ((val & 0x7F) as u8) | 0x80;
        val >>= 7;
        pos += 1;
    }
    out[pos] = val as u8;
    pos + 1
}

pub(super) const fn str_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    varint_usize_len(bytes.len()) + bytes.len()
}

pub(super) const fn write_str(s: &str, out: &mut [u8], cursor: usize) -> usize {
    let bytes = s.as_bytes();
    let mut pos = write_varint_usize(bytes.len(), out, cursor);
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
pub(super) const fn option_str_len(s: &Option<Cow<'static, str>>) -> usize {
    match s {
        None => 1,
        Some(inner) => 1 + str_len(cow_str_as_str(inner)),
    }
}

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
