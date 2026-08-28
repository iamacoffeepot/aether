//! Per-type value walk that flattens nested structs and enums into
//! TLV leaves (ADR-0059).
//!
//! Blanket impls cover the closed vocabulary: scalars, `bool`, owned
//! strings, `Bytes`, and the typed-id newtypes. The containers `Vec`,
//! `Map`, and `Array` are one record each whose body encoding the
//! element type selects through [`StorageElement`]. User structs and
//! enums get a derive-emitted impl. `Option<T>` is the two-variant
//! enum, not a special case.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::element::{StorageElement, container_hash};
use super::hash::{
    BYTES_SCHEMA, MAX_STORAGE_DEPTH, U64_SCHEMA, UNIT_SCHEMA, VARIANT_LEAF, fold_path_segment, terminate_field_hash,
    variant_hash,
};
use super::leaf::{LeafBody, decode_leaf, encode_leaf};
use super::record::{RecordReader, RecordWriter, StorageError};
use crate::{DagId, KindId, MailboxId, Schema, ThreadId, TransformId};

/// Contribute this value's leaves under a parent path carry, and
/// reassemble this value from a matched record set.
pub trait StorageLeaves: Sized {
    /// Write every schema-declared leaf of `self` under `carry`.
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError>;

    /// Rebuild `Self` from records under `carry`. Required-leaf
    /// absence is [`StorageError::MissingRequiredField`]; an `Option`
    /// whose `__variant` is missing decodes to `None`.
    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError>;

    /// True when no leaf under this path is present on the wire
    /// (sender-side version skew), so a read alias can be tried.
    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool;
}

fn check_depth(depth: u32) -> Result<(), StorageError> {
    if depth > MAX_STORAGE_DEPTH {
        Err(StorageError::NestingTooDeep)
    } else {
        Ok(())
    }
}

fn emit_leaf<T: LeafBody>(hash: u64, value: &T, sink: &mut RecordWriter) -> Result<(), StorageError> {
    let mut body = Vec::new();
    encode_leaf(value, &mut body)?;
    sink.emit(hash, body)
}

fn take_required<T: LeafBody>(hash: u64, name: &'static str, source: &mut RecordReader) -> Result<T, StorageError> {
    let body = source.take(hash).ok_or(StorageError::MissingRequiredField { hash, name })?;
    decode_leaf(&body)
}

fn contribute_opaque<T: LeafBody + Schema>(
    value: &T,
    carry: u64,
    depth: u32,
    sink: &mut RecordWriter,
) -> Result<(), StorageError> {
    check_depth(depth)?;
    emit_leaf(terminate_field_hash(carry, &T::SCHEMA), value, sink)
}

fn assemble_opaque<T: LeafBody + Schema>(carry: u64, depth: u32, source: &mut RecordReader) -> Result<T, StorageError> {
    check_depth(depth)?;
    take_required(terminate_field_hash(carry, &T::SCHEMA), "", source)
}

fn opaque_absent<T: Schema>(carry: u64, source: &RecordReader) -> bool {
    !source.contains(terminate_field_hash(carry, &T::SCHEMA))
}

macro_rules! opaque_leaf {
    ($($t:ty),+ $(,)?) => {
        $(
            impl StorageLeaves for $t {
                fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
                    contribute_opaque(self, carry, depth, sink)
                }

                fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
                    assemble_opaque(carry, depth, source)
                }

                fn is_absent(carry: u64, _depth: u32, source: &RecordReader) -> bool {
                    opaque_absent::<Self>(carry, source)
                }
            }
        )+
    };
}

opaque_leaf!(
    u8,
    u16,
    u32,
    u64,
    i8,
    i16,
    i32,
    i64,
    f32,
    f64,
    bool,
    (),
    String,
    MailboxId,
    KindId,
    DagId,
    TransformId,
    ThreadId,
);

impl<T: StorageElement + Schema + 'static> StorageLeaves for Vec<T>
where
    Self: StorageElement + Schema,
{
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_container(self, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_container(carry, depth, source)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        !source.contains(container_hash::<Self>(carry, depth))
    }
}

impl<T: StorageElement + Schema + 'static, const N: usize> StorageLeaves for [T; N]
where
    Self: StorageElement + Schema,
{
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_container(self, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_container(carry, depth, source)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        !source.contains(container_hash::<Self>(carry, depth))
    }
}

impl<K: StorageElement + Schema + Ord + 'static, V: StorageElement + Schema + 'static> StorageLeaves for BTreeMap<K, V>
where
    Self: StorageElement + Schema,
{
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_container(self, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_container(carry, depth, source)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        !source.contains(container_hash::<Self>(carry, depth))
    }
}

/// One container field as one record: the tag from [`container_hash`],
/// the body from the container's own element encoding. Positional
/// content reproduces the retired opaque form byte for byte; tagged
/// content gets the schema-independent tag and framed element bodies.
fn contribute_container<C: StorageElement + Schema>(
    value: &C,
    carry: u64,
    depth: u32,
    sink: &mut RecordWriter,
) -> Result<(), StorageError> {
    check_depth(depth)?;
    let mut body = Vec::new();
    value.contribute_element(depth, &mut body)?;
    sink.emit(container_hash::<C>(carry, depth), body)
}

/// Inverse of [`contribute_container`], requiring the record body fully
/// consumed.
fn assemble_container<C: StorageElement + Schema>(
    carry: u64,
    depth: u32,
    source: &mut RecordReader,
) -> Result<C, StorageError> {
    check_depth(depth)?;
    let hash = container_hash::<C>(carry, depth);
    let bytes = source.take(hash).ok_or(StorageError::MissingRequiredField { hash, name: "" })?;
    let mut cursor = bytes.as_slice();
    let value = C::assemble_element(depth, &mut cursor)?;
    if cursor.is_empty() {
        Ok(value)
    } else {
        Err(StorageError::TrailingBytes)
    }
}

/// Bytes-shaped `Vec<u8>` field, hashed against [`BYTES_SCHEMA`] so it
/// agrees with the `#[derive(Schema)]` `Vec<u8>` specialization.
pub fn contribute_bytes(bytes: &[u8], carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
    check_depth(depth)?;
    let owned = bytes.to_vec();
    emit_leaf(terminate_field_hash(carry, &BYTES_SCHEMA), &owned, sink)
}

/// Inverse of [`contribute_bytes`].
pub fn assemble_bytes(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Vec<u8>, StorageError> {
    check_depth(depth)?;
    take_required(terminate_field_hash(carry, &BYTES_SCHEMA), "", source)
}

/// Absence probe matching [`contribute_bytes`].
#[must_use]
pub fn bytes_absent(carry: u64, source: &RecordReader) -> bool {
    !source.contains(terminate_field_hash(carry, &BYTES_SCHEMA))
}

impl<T: StorageLeaves + Schema + 'static> StorageLeaves for Option<T> {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        check_depth(depth)?;
        let var_hash = terminate_field_hash(fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth), &U64_SCHEMA);
        match self {
            None => emit_leaf(var_hash, &none_hash(), sink),
            Some(value) => {
                emit_leaf(var_hash, &some_hash::<T>(), sink)?;
                let some_carry = fold_path_segment(carry, b"Some", depth);
                value.contribute(some_carry, depth + 1, sink)
            }
        }
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        check_depth(depth)?;
        let var_hash = terminate_field_hash(fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth), &U64_SCHEMA);
        let Some(body) = source.take(var_hash) else {
            return Ok(None);
        };
        let disc: u64 = decode_leaf(&body)?;
        if disc == none_hash() {
            Ok(None)
        } else if disc == some_hash::<T>() {
            let some_carry = fold_path_segment(carry, b"Some", depth);
            Ok(Some(T::assemble(some_carry, depth + 1, source)?))
        } else {
            Err(StorageError::UnknownVariant { hash: disc })
        }
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        let var_hash = terminate_field_hash(fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth), &U64_SCHEMA);
        !source.contains(var_hash)
    }
}

fn none_hash() -> u64 {
    variant_hash("None", &UNIT_SCHEMA)
}

fn some_hash<T: Schema>() -> u64 {
    variant_hash("Some", &T::SCHEMA)
}

/// Assemble a `Vec<u8>` Bytes leaf, falling through alias carries when
/// the primary path is absent.
pub fn assemble_bytes_with_aliases(
    primary: u64,
    aliases: &[u64],
    depth: u32,
    source: &mut RecordReader,
) -> Result<Vec<u8>, StorageError> {
    if bytes_absent(primary, source) {
        for alias in aliases {
            if !bytes_absent(*alias, source) {
                return assemble_bytes(*alias, depth, source);
            }
        }
    }
    assemble_bytes(primary, depth, source)
}

/// Assemble `T` from `primary`, falling through each alias carry when
/// the primary (or previous alias) is fully absent.
pub fn assemble_with_aliases<T: StorageLeaves>(
    primary: u64,
    aliases: &[u64],
    depth: u32,
    source: &mut RecordReader,
) -> Result<T, StorageError> {
    if T::is_absent(primary, depth, source) {
        for alias in aliases {
            if !T::is_absent(*alias, depth, source) {
                return T::assemble(*alias, depth, source);
            }
        }
    }
    T::assemble(primary, depth, source)
}
