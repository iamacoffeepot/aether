//! Const field- and variant-hash primitives (ADR-0059).
//!
//! A leaf's dotted path is never materialized. The parent hands each
//! child an in-progress carry and the child folds its own segment onto
//! it, the way [`crate::mailbox_id_from_name_pair`] folds a prefixed
//! segment. The preimage is `FIELD_DOMAIN`, then the path bytes, then a
//! `0x00` terminator, then the type's canonical schema bytes. Rust
//! identifiers and the dot join both exclude NUL, so the boundary is
//! unambiguous and the fold stays incremental and const.

// clippy's `ptr_arg` wants `&[T]` / `&str` over `&Cow<[T]>` / `&Cow<str>`.
// Deref of `Cow` is not `const`, so these helpers match on the variant
// to narrow `Cow::Borrowed` by hand — the same exemption
// `canonical::primitives` documents.
#![allow(clippy::ptr_arg)] // aether-suppression-request: Cow deref is not const; helpers match Borrowed by hand like canonical::primitives

use alloc::borrow::Cow;

use crate::hash::{FIELD_DOMAIN, VARIANT_DOMAIN, fnv1a_64_fold, fnv1a_64_prefixed};
use crate::schema::{EnumVariant, NamedField, Primitive, SchemaCell, SchemaType};

/// Depth at which a pathological schema fails const-evaluation rather
/// than overflowing the stack. Nested user types in practice sit well
/// below this; the cap exists because the walk is recursive.
pub const MAX_STORAGE_DEPTH: u32 = 32;

/// Path segment naming the enum discriminant leaf (`<path>.__variant`).
pub const VARIANT_LEAF: &str = "__variant";

/// NUL that separates the dotted path from the canonical type bytes in
/// a field-hash preimage.
const PATH_TERMINATOR: u8 = 0x00;

const SCHEMA_UNIT: u32 = 0;
const SCHEMA_BOOL: u32 = 1;
const SCHEMA_SCALAR: u32 = 2;
const SCHEMA_STRING: u32 = 3;
const SCHEMA_BYTES: u32 = 4;
const SCHEMA_OPTION: u32 = 5;
const SCHEMA_VEC: u32 = 6;
const SCHEMA_ARRAY: u32 = 7;
const SCHEMA_STRUCT: u32 = 8;
const SCHEMA_ENUM: u32 = 9;
const SCHEMA_MAP: u32 = 10;
const SCHEMA_TYPE_ID: u32 = 11;

const VARIANT_UNIT: u32 = 0;
const VARIANT_TUPLE: u32 = 1;
const VARIANT_STRUCT: u32 = 2;

/// Carry at the root of a storage kind: `FIELD_DOMAIN` folded onto the
/// FNV-1a offset, with no path segments yet.
#[must_use]
pub const fn field_path_root() -> u64 {
    fnv1a_64_prefixed(FIELD_DOMAIN, b"")
}

/// Fold one path segment onto `carry`. Depth zero writes the segment
/// bytes alone; deeper segments write a `.` then the segment, matching
/// the dotted-path preimage without allocating the joined string.
#[must_use]
pub const fn fold_path_segment(carry: u64, segment: &[u8], depth: u32) -> u64 {
    if depth == 0 {
        fnv1a_64_fold(carry, segment)
    } else {
        fnv1a_64_fold(fnv1a_64_fold(carry, b"."), segment)
    }
}

/// Fold a decimal index (`0`, `1`, …) as a path segment. Used for
/// multi-field tuple variants.
#[must_use]
pub const fn fold_index_segment(carry: u64, depth: u32, index: usize) -> u64 {
    let mut buf = [0u8; 20];
    let start = write_decimal(&mut buf, index);
    fold_path_segment(carry, buf.split_at(start).1, depth)
}

const DIGITS: &[u8; 10] = b"0123456789";

const fn write_decimal(buf: &mut [u8; 20], mut n: usize) -> usize {
    if n == 0 {
        buf[19] = b'0';
        return 19;
    }
    let mut i = 20;
    while n > 0 {
        i -= 1;
        buf[i] = DIGITS[n % 10];
        n /= 10;
    }
    i
}

/// Terminate a path carry with `0x00` and fold the type's canonical
/// schema bytes. The result is the field hash for that leaf.
#[must_use]
pub const fn terminate_field_hash(carry: u64, schema: &SchemaType) -> u64 {
    fold_canonical_schema(fnv1a_64_fold(carry, &[PATH_TERMINATOR]), schema, 0)
}

/// Field hash of a dotted path under `FIELD_DOMAIN`. `path` is one or
/// more `.`-joined segments; the empty path hashes the type at the root.
#[must_use]
pub const fn field_hash(path: &str, schema: &SchemaType) -> u64 {
    terminate_field_hash(fold_dotted_path(field_path_root(), path), schema)
}

/// Fold a `.`-joined path onto `carry`. Depth counts segments already
/// present in `carry`; a caller that has only the domain root passes
/// the path as written.
#[must_use]
pub const fn fold_dotted_path(mut carry: u64, path: &str) -> u64 {
    if path.is_empty() {
        return carry;
    }
    let bytes = path.as_bytes();
    let mut start = 0;
    let mut depth = 0;
    let mut i = 0;
    while i <= bytes.len() {
        if i == bytes.len() || bytes[i] == b'.' {
            carry = fold_path_segment(carry, bytes.split_at(i).0.split_at(start).1, depth);
            depth += 1;
            start = i + 1;
        }
        i += 1;
    }
    carry
}

/// Variant discriminant hash under `VARIANT_DOMAIN`. `body` is the
/// canonical schema of the variant payload: `Unit` for a unit variant,
/// the inner type for a single-field tuple, or a nameless `Struct` of
/// the remaining fields.
#[must_use]
pub const fn variant_hash(name: &str, body: &SchemaType) -> u64 {
    let hash = fnv1a_64_prefixed(VARIANT_DOMAIN, name.as_bytes());
    fold_canonical_schema(fnv1a_64_fold(hash, &[PATH_TERMINATOR]), body, 0)
}

/// Fold the canonical schema encoding of `schema` onto `hash`. The
/// bytes match [`crate::canonical::canonical_serialize_schema`]; the
/// walk is incremental so a const caller never materializes the array.
const fn fold_canonical_schema(hash: u64, schema: &SchemaType, depth: u32) -> u64 {
    assert!(depth <= MAX_STORAGE_DEPTH, "storage hash: schema nesting exceeds MAX_STORAGE_DEPTH");
    match schema {
        SchemaType::Unit => fold_u32(hash, SCHEMA_UNIT),
        SchemaType::Bool => fold_u32(hash, SCHEMA_BOOL),
        SchemaType::Scalar(p) => fold_u32(fold_u32(hash, SCHEMA_SCALAR), primitive_tag(*p)),
        SchemaType::String => fold_u32(hash, SCHEMA_STRING),
        SchemaType::Bytes => fold_u32(hash, SCHEMA_BYTES),
        SchemaType::Option(cell) => {
            fold_canonical_schema(fold_u32(hash, SCHEMA_OPTION), schema_of_cell(cell), depth + 1)
        }
        SchemaType::Vec(cell) => fold_canonical_schema(fold_u32(hash, SCHEMA_VEC), schema_of_cell(cell), depth + 1),
        SchemaType::Array { element, len } => {
            let hash = fold_canonical_schema(fold_u32(hash, SCHEMA_ARRAY), schema_of_cell(element), depth + 1);
            fold_u32(hash, *len)
        }
        SchemaType::Struct { fields, repr_c } => {
            let slice = named_fields(fields);
            let mut hash = fold_u32(fold_u32(hash, SCHEMA_STRUCT), count_u32(slice.len()));
            let mut i = 0;
            while i < slice.len() {
                hash = fold_canonical_schema(hash, &slice[i].ty, depth + 1);
                i += 1;
            }
            fold_u8(
                hash,
                if *repr_c {
                    1
                } else {
                    0
                },
            )
        }
        SchemaType::Enum { variants } => {
            let slice = enum_variants(variants);
            let mut hash = fold_u32(fold_u32(hash, SCHEMA_ENUM), count_u32(slice.len()));
            let mut i = 0;
            while i < slice.len() {
                hash = fold_canonical_variant(hash, &slice[i], depth + 1);
                i += 1;
            }
            hash
        }
        SchemaType::Map { key, value } => {
            let hash = fold_canonical_schema(fold_u32(hash, SCHEMA_MAP), schema_of_cell(key), depth + 1);
            fold_canonical_schema(hash, schema_of_cell(value), depth + 1)
        }
        SchemaType::TypeId(id) => fold_u64(fold_u32(hash, SCHEMA_TYPE_ID), *id),
    }
}

const fn fold_canonical_variant(hash: u64, variant: &EnumVariant, depth: u32) -> u64 {
    match variant {
        EnumVariant::Unit { discriminant, .. } => fold_u32(fold_u32(hash, VARIANT_UNIT), *discriminant),
        EnumVariant::Tuple { discriminant, fields, .. } => {
            let slice = schema_types(fields);
            let mut hash = fold_u32(fold_u32(fold_u32(hash, VARIANT_TUPLE), *discriminant), count_u32(slice.len()));
            let mut i = 0;
            while i < slice.len() {
                hash = fold_canonical_schema(hash, &slice[i], depth);
                i += 1;
            }
            hash
        }
        EnumVariant::Struct { discriminant, fields, .. } => {
            let slice = named_fields(fields);
            let mut hash = fold_u32(fold_u32(fold_u32(hash, VARIANT_STRUCT), *discriminant), count_u32(slice.len()));
            let mut i = 0;
            while i < slice.len() {
                hash = fold_canonical_schema(hash, &slice[i].ty, depth);
                i += 1;
            }
            hash
        }
    }
}

const fn fold_u8(hash: u64, byte: u8) -> u64 {
    fnv1a_64_fold(hash, &[byte])
}

const fn fold_u32(hash: u64, val: u32) -> u64 {
    fnv1a_64_fold(hash, &val.to_le_bytes())
}

const fn fold_u64(hash: u64, val: u64) -> u64 {
    fnv1a_64_fold(hash, &val.to_le_bytes())
}

#[allow(clippy::cast_possible_truncation)] // aether-suppression-request: const count after assert that len fits u32; try_from is not const
const fn count_u32(len: usize) -> u32 {
    assert!(len <= u32::MAX as usize, "storage hash: count exceeds u32::MAX");
    len as u32
}

const fn primitive_tag(p: Primitive) -> u32 {
    match p {
        Primitive::U8 => 0,
        Primitive::U16 => 1,
        Primitive::U32 => 2,
        Primitive::U64 => 3,
        Primitive::I8 => 4,
        Primitive::I16 => 5,
        Primitive::I32 => 6,
        Primitive::I64 => 7,
        Primitive::F32 => 8,
        Primitive::F64 => 9,
    }
}

const fn schema_of_cell(cell: &SchemaCell) -> &SchemaType {
    match cell {
        SchemaCell::Static(r) => r,
        SchemaCell::Owned(_) => panic!("storage hash: Owned SchemaCell not supported in const context"),
    }
}

const fn named_fields<'a>(fields: &'a Cow<'static, [NamedField]>) -> &'a [NamedField] {
    match fields {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("storage hash: Owned Cow<[NamedField]> not supported in const context"),
    }
}

const fn enum_variants<'a>(variants: &'a Cow<'static, [EnumVariant]>) -> &'a [EnumVariant] {
    match variants {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("storage hash: Owned Cow<[EnumVariant]> not supported in const context"),
    }
}

const fn schema_types<'a>(fields: &'a Cow<'static, [SchemaType]>) -> &'a [SchemaType] {
    match fields {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("storage hash: Owned Cow<[SchemaType]> not supported in const context"),
    }
}

/// Walk `schema` under `carry` and return the number of TLV leaves the
/// flattening rules emit, including every enum variant (collision
/// check sees the full set, not the active arm).
///
/// # Panics
/// Panics if `depth` exceeds [`MAX_STORAGE_DEPTH`], or if `schema`
/// contains an `Owned` cell — only derive-emitted `Static` schemas are
/// legal in const context.
#[must_use]
pub const fn count_leaves(schema: &SchemaType, carry: u64, depth: u32) -> usize {
    assert!(depth <= MAX_STORAGE_DEPTH, "storage hash: schema nesting exceeds MAX_STORAGE_DEPTH");
    match schema {
        SchemaType::Struct { fields, .. } => {
            let slice = named_fields(fields);
            let mut total = 0;
            let mut i = 0;
            while i < slice.len() {
                let child = fold_path_segment(carry, cow_str_bytes(&slice[i].name), depth);
                total += count_leaves(&slice[i].ty, child, depth + 1);
                i += 1;
            }
            total
        }
        SchemaType::Enum { variants } => {
            let slice = enum_variants(variants);
            let mut total = 1;
            let mut i = 0;
            while i < slice.len() {
                total += count_variant_leaves(&slice[i], carry, depth);
                i += 1;
            }
            total
        }
        SchemaType::Option(cell) => {
            let some = fold_path_segment(carry, b"Some", depth);
            1 + count_leaves(schema_of_cell(cell), some, depth + 1)
        }
        SchemaType::Unit
        | SchemaType::Bool
        | SchemaType::Scalar(_)
        | SchemaType::String
        | SchemaType::Bytes
        | SchemaType::Vec(_)
        | SchemaType::Array { .. }
        | SchemaType::Map { .. }
        | SchemaType::TypeId(_) => 1,
    }
}

const fn count_variant_leaves(variant: &EnumVariant, carry: u64, depth: u32) -> usize {
    let name = variant_name(variant);
    let child = fold_path_segment(carry, name.as_bytes(), depth);
    match variant {
        EnumVariant::Unit { .. } => 0,
        EnumVariant::Tuple { fields, .. } => {
            let slice = schema_types(fields);
            if slice.len() == 1 {
                count_leaves(&slice[0], child, depth + 1)
            } else {
                let mut total = 0;
                let mut i = 0;
                while i < slice.len() {
                    let indexed = fold_index_segment(child, depth + 1, i);
                    total += count_leaves(&slice[i], indexed, depth + 2);
                    i += 1;
                }
                total
            }
        }
        EnumVariant::Struct { fields, .. } => {
            let slice = named_fields(fields);
            let mut total = 0;
            let mut i = 0;
            while i < slice.len() {
                let nested = fold_path_segment(child, cow_str_bytes(&slice[i].name), depth + 1);
                total += count_leaves(&slice[i].ty, nested, depth + 2);
                i += 1;
            }
            total
        }
    }
}

const fn variant_name(variant: &EnumVariant) -> &str {
    match variant {
        EnumVariant::Unit { name, .. } | EnumVariant::Tuple { name, .. } | EnumVariant::Struct { name, .. } => {
            cow_str(name)
        }
    }
}

const fn cow_str<'a>(c: &'a Cow<'static, str>) -> &'a str {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("storage hash: Owned Cow<str> not supported in const context"),
    }
}

const fn cow_str_bytes<'a>(c: &'a Cow<'static, str>) -> &'a [u8] {
    cow_str(c).as_bytes()
}

/// Hash of the `index`th flattened leaf of `schema` under `carry`, in
/// walk order. Used with [`count_leaves`] for the pairwise collision
/// check the derive emits.
///
/// # Panics
/// Panics if `index` is out of range, if `depth` exceeds
/// [`MAX_STORAGE_DEPTH`], or if `schema` contains an `Owned` cell.
#[must_use]
pub const fn nth_leaf_hash(schema: &SchemaType, carry: u64, depth: u32, index: usize) -> u64 {
    match find_nth_leaf(schema, carry, depth, index) {
        Some(hash) => hash,
        None => panic!("storage hash: leaf index out of range"),
    }
}

const fn find_nth_leaf(schema: &SchemaType, carry: u64, depth: u32, index: usize) -> Option<u64> {
    assert!(depth <= MAX_STORAGE_DEPTH, "storage hash: schema nesting exceeds MAX_STORAGE_DEPTH");
    match schema {
        SchemaType::Struct { fields, .. } => {
            let slice = named_fields(fields);
            let mut remaining = index;
            let mut i = 0;
            while i < slice.len() {
                let child = fold_path_segment(carry, cow_str_bytes(&slice[i].name), depth);
                let n = count_leaves(&slice[i].ty, child, depth + 1);
                if remaining < n {
                    return find_nth_leaf(&slice[i].ty, child, depth + 1, remaining);
                }
                remaining -= n;
                i += 1;
            }
            None
        }
        SchemaType::Enum { variants } => {
            if index == 0 {
                return Some(terminate_field_hash(
                    fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth),
                    &U64_SCHEMA,
                ));
            }
            let slice = enum_variants(variants);
            let mut remaining = index - 1;
            let mut i = 0;
            while i < slice.len() {
                let n = count_variant_leaves(&slice[i], carry, depth);
                if remaining < n {
                    return nth_variant_leaf(&slice[i], carry, depth, remaining);
                }
                remaining -= n;
                i += 1;
            }
            None
        }
        SchemaType::Option(cell) => {
            if index == 0 {
                return Some(terminate_field_hash(
                    fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth),
                    &U64_SCHEMA,
                ));
            }
            let some = fold_path_segment(carry, b"Some", depth);
            find_nth_leaf(schema_of_cell(cell), some, depth + 1, index - 1)
        }
        SchemaType::Unit
        | SchemaType::Bool
        | SchemaType::Scalar(_)
        | SchemaType::String
        | SchemaType::Bytes
        | SchemaType::Vec(_)
        | SchemaType::Array { .. }
        | SchemaType::Map { .. }
        | SchemaType::TypeId(_) => {
            if index == 0 {
                Some(terminate_field_hash(carry, schema))
            } else {
                None
            }
        }
    }
}

const fn nth_variant_leaf(variant: &EnumVariant, carry: u64, depth: u32, index: usize) -> Option<u64> {
    let name = variant_name(variant);
    let child = fold_path_segment(carry, name.as_bytes(), depth);
    match variant {
        EnumVariant::Unit { .. } => None,
        EnumVariant::Tuple { fields, .. } => {
            let slice = schema_types(fields);
            if slice.len() == 1 {
                find_nth_leaf(&slice[0], child, depth + 1, index)
            } else {
                let mut remaining = index;
                let mut i = 0;
                while i < slice.len() {
                    let indexed = fold_index_segment(child, depth + 1, i);
                    let n = count_leaves(&slice[i], indexed, depth + 2);
                    if remaining < n {
                        return find_nth_leaf(&slice[i], indexed, depth + 2, remaining);
                    }
                    remaining -= n;
                    i += 1;
                }
                None
            }
        }
        EnumVariant::Struct { fields, .. } => {
            let slice = named_fields(fields);
            let mut remaining = index;
            let mut i = 0;
            while i < slice.len() {
                let nested = fold_path_segment(child, cow_str_bytes(&slice[i].name), depth + 1);
                let n = count_leaves(&slice[i].ty, nested, depth + 2);
                if remaining < n {
                    return find_nth_leaf(&slice[i].ty, nested, depth + 2, remaining);
                }
                remaining -= n;
                i += 1;
            }
            None
        }
    }
}

/// `u64` schema used for `__variant` discriminant leaves. Kept as a
/// const so the collision walk and the value walk hash the same type.
pub const U64_SCHEMA: SchemaType = SchemaType::Scalar(Primitive::U64);

/// `Bytes` schema used when a `Vec<u8>` field is specialized the same
/// way `#[derive(Schema)]` specializes it.
pub const BYTES_SCHEMA: SchemaType = SchemaType::Bytes;

/// `Unit` schema for unit variants and empty structs.
pub const UNIT_SCHEMA: SchemaType = SchemaType::Unit;

/// Pairwise uniqueness over the fully flattened leaf set of `schema`
/// plus every alias prefix. Panics at const-eval on a collision so the
/// derive surfaces it as a compile error.
///
/// `aliases` is `(old_field_name, current_field_schema)` pairs, each
/// flattened as if the field still used the old name.
///
/// # Panics
/// Panics on a within-kind leaf-hash collision or a read-alias
/// collision (ADR-0059 rule 2), and on the same depth / `Owned`-cell
/// faults [`count_leaves`] names.
pub const fn assert_unique_storage_leaves(schema: &SchemaType, aliases: &[(&str, &SchemaType)]) {
    let live = count_leaves(schema, field_path_root(), 0);
    let mut extra = 0;
    let mut a = 0;
    while a < aliases.len() {
        let carry = fold_path_segment(field_path_root(), aliases[a].0.as_bytes(), 0);
        extra += count_leaves(aliases[a].1, carry, 1);
        a += 1;
    }
    let total = live + extra;
    let mut i = 0;
    while i < total {
        let left = hash_at(schema, aliases, live, i);
        let mut j = i + 1;
        while j < total {
            let right = hash_at(schema, aliases, live, j);
            if left == right {
                assert!(
                    !(i < live && j < live),
                    "storage kind has a within-kind leaf hash collision (ADR-0059 rule 2)"
                );
                panic!("storage kind has a read-alias hash collision (ADR-0059 rule 2)");
            }
            j += 1;
        }
        i += 1;
    }
}

const fn hash_at(schema: &SchemaType, aliases: &[(&str, &SchemaType)], live: usize, index: usize) -> u64 {
    if index < live {
        return nth_leaf_hash(schema, field_path_root(), 0, index);
    }
    let mut rest = index - live;
    let mut a = 0;
    while a < aliases.len() {
        let carry = fold_path_segment(field_path_root(), aliases[a].0.as_bytes(), 0);
        let n = count_leaves(aliases[a].1, carry, 1);
        if rest < n {
            return nth_leaf_hash(aliases[a].1, carry, 1, rest);
        }
        rest -= n;
        a += 1;
    }
    panic!("storage hash: alias leaf index out of range")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::Schema;
    use crate::canonical::{canonical_len_schema, canonical_serialize_schema};
    use crate::hash::{FIELD_DOMAIN, fnv1a_64_bytes, fnv1a_64_fold, fnv1a_64_prefixed};
    use crate::schema::Primitive;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn materialized(path: &str, schema: &SchemaType) -> u64 {
        let n = canonical_len_schema(schema);
        let mut bytes = Vec::from(FIELD_DOMAIN);
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(PATH_TERMINATOR);
        match n {
            4 => bytes.extend_from_slice(&canonical_serialize_schema::<4>(schema)),
            8 => bytes.extend_from_slice(&canonical_serialize_schema::<8>(schema)),
            12 => bytes.extend_from_slice(&canonical_serialize_schema::<12>(schema)),
            16 => bytes.extend_from_slice(&canonical_serialize_schema::<16>(schema)),
            20 => bytes.extend_from_slice(&canonical_serialize_schema::<20>(schema)),
            24 => bytes.extend_from_slice(&canonical_serialize_schema::<24>(schema)),
            other => panic!("test helper: unexpected canonical len {other}"),
        }
        fnv1a_64_bytes(&bytes)
    }

    #[test]
    fn field_hash_fold_matches_joined_preimage() {
        // Tripwire: folding an unmaterialized dotted path must match
        // hashing FIELD_DOMAIN ++ path ++ NUL ++ canonical bytes as one
        // buffer. If the separator or domain prefix drifts, every stored
        // row's field tags move.
        let schema = &<u64 as Schema>::SCHEMA;
        assert_eq!(field_hash("id", schema), materialized("id", schema));
        assert_eq!(field_hash("addr.street", schema), materialized("addr.street", schema));
        let string = &<String as Schema>::SCHEMA;
        assert_eq!(field_hash("addr.street", string), materialized("addr.street", string));
    }

    #[test]
    fn dotted_path_fold_matches_segment_walk() {
        let mut carry = field_path_root();
        carry = fold_path_segment(carry, b"addr", 0);
        carry = fold_path_segment(carry, b"street", 1);
        assert_eq!(carry, fold_dotted_path(field_path_root(), "addr.street"));
        assert_eq!(
            terminate_field_hash(carry, &<u64 as Schema>::SCHEMA),
            field_hash("addr.street", &<u64 as Schema>::SCHEMA),
        );
    }

    #[test]
    fn terminate_appends_nul_then_canonical_bytes() {
        static SCHEMA: SchemaType = SchemaType::Scalar(Primitive::U64);
        const N: usize = canonical_len_schema(&SCHEMA);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&SCHEMA);
        let mut expected = fnv1a_64_prefixed(FIELD_DOMAIN, b"x");
        expected = fnv1a_64_fold(expected, &[PATH_TERMINATOR]);
        expected = fnv1a_64_fold(expected, &BYTES);
        assert_eq!(field_hash("x", &SCHEMA), expected);
    }
}
