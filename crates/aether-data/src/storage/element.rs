//! Container-element encoding (ADR-0059 containers, #5496).
//!
//! A container field is still one TLV record; what varies is the element
//! body, selected by the element type's own derive through
//! [`StorageElement`]. A `#[derive(Schema)]` element contributes its
//! positional wire bytes — byte-identical to the opaque container form
//! this replaces — and carries no tolerance promise. A
//! `#[derive(Storage)]` element contributes a length-framed record
//! stream rooted at the element type, so element schema drift decodes
//! the way root-level drift does: unknown fields skip, missing
//! optionals default, missing required fields refuse by name.
//!
//! The record tag differs with the element class. Positional containers
//! keep the schema-folded hash (drift-refusal is their semantic and the
//! bytes predate this module). Tagged containers fold the reserved
//! [`ELEMENTS_LEAF`] segment instead and terminate against
//! [`BYTES_SCHEMA`], so evolving the element type never moves the
//! container's own tag. Flipping an element type between the two
//! classes is therefore an ordinary breaking schema change (ADR-0187):
//! the tag moves, and the old rows owe a named upcast.
//!
//! Element-level unknown fields have no side-channel on a plain value,
//! so a rewrite by an older binary sheds fields a newer writer added
//! inside tagged elements. Reads stay tolerant; rewrites shed. The
//! root-level unknown bucket ([`super::StorageData`]) is unaffected.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::hash::{BYTES_SCHEMA, field_path_root, fold_path_segment, terminate_field_hash};
use super::leaf::{LeafBody, decode_stream_leaf, encode_leaf};
use super::leaves::StorageLeaves;
use super::record::{RecordReader, RecordWriter, StorageError};
use crate::wire::Error as WireError;
use crate::{DagId, KindId, MailboxId, Schema, ThreadId, TransformId};

/// Reserved path segment folded into a tagged container's record tag in
/// place of the container schema. Shares the `__` synthesis prefix with
/// [`super::VARIANT_LEAF`]; the derive refuses user fields under it.
pub const ELEMENTS_LEAF: &str = "__elements";

/// How a value encodes as one element inside a container record body.
///
/// Exactly one impl exists per type, emitted by the type's derive:
/// `#[derive(Schema)]` emits the positional form, `#[derive(Storage)]`
/// the tagged form, and the closed leaf vocabulary is enumerated below.
/// There is deliberately no blanket impl over the wire codec — the
/// derive a type declares is the selector, and coherence keeps it
/// honest.
pub trait StorageElement: Sized {
    /// True when the body layout is independent of the element's
    /// schema, so element evolution leaves sibling bytes decodable.
    const TAGGED: bool;

    /// Append this value's element body to `out`.
    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError>;

    /// Consume this value's element body from the front of `cursor`.
    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError>;
}

/// The record tag for a container field: schema-folded for positional
/// content, [`ELEMENTS_LEAF`]-folded for tagged content.
#[must_use]
pub fn container_hash<C: Schema + StorageElement>(carry: u64, depth: u32) -> u64 {
    if C::TAGGED {
        terminate_field_hash(fold_path_segment(carry, ELEMENTS_LEAF.as_bytes(), depth), &BYTES_SCHEMA)
    } else {
        terminate_field_hash(carry, &C::SCHEMA)
    }
}

/// Tagged element body: a `u32` length frame around the value's own
/// record stream, rooted at the element type. Emitted by
/// `#[derive(Storage)]`.
pub fn contribute_tagged_element<T: StorageLeaves>(
    value: &T,
    depth: u32,
    out: &mut Vec<u8>,
) -> Result<(), StorageError> {
    let mut writer = RecordWriter::new();
    value.contribute(field_path_root(), depth, &mut writer)?;
    let stream = writer.finish()?;
    let length = u32::try_from(stream.len()).map_err(|_| StorageError::LeafBody(WireError::Length))?;
    encode_leaf(&length, out)?;
    out.extend_from_slice(&stream);
    Ok(())
}

/// Inverse of [`contribute_tagged_element`]. Unknown fields inside the
/// frame are shed — see the module doc.
pub fn assemble_tagged_element<T: StorageLeaves>(depth: u32, cursor: &mut &[u8]) -> Result<T, StorageError> {
    let length = decode_stream_leaf::<u32>(cursor)? as usize;
    if cursor.len() < length {
        return Err(StorageError::TrailingBytes);
    }
    let (frame, rest) = cursor.split_at(length);
    *cursor = rest;
    T::assemble(field_path_root(), depth, &mut RecordReader::parse(frame)?)
}

/// Positional element body: the value's ordinary wire bytes. Emitted
/// by `#[derive(Schema)]`.
pub fn contribute_positional_element<T: LeafBody>(value: &T, out: &mut Vec<u8>) -> Result<(), StorageError> {
    encode_leaf(value, out)
}

/// Inverse of [`contribute_positional_element`]; consumes from the
/// front of `cursor`.
pub fn assemble_positional_element<T: LeafBody>(cursor: &mut &[u8]) -> Result<T, StorageError> {
    decode_stream_leaf(cursor)
}

macro_rules! positional_element {
    ($($t:ty),+ $(,)?) => {
        $(
            impl StorageElement for $t {
                const TAGGED: bool = false;

                fn contribute_element(&self, _depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
                    encode_leaf(self, out)
                }

                fn assemble_element(_depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
                    decode_stream_leaf(cursor)
                }
            }
        )+
    };
}

positional_element!(
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

impl<T: StorageElement> StorageElement for Vec<T> {
    const TAGGED: bool = T::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        contribute_count(self.len(), out)?;
        for item in self {
            item.contribute_element(depth, out)?;
        }
        Ok(())
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        let count = decode_stream_leaf::<u32>(cursor)? as usize;
        let mut items = Self::with_capacity(count.min(cursor.len()));
        for _ in 0..count {
            items.push(T::assemble_element(depth, cursor)?);
        }
        Ok(items)
    }
}

impl<T: StorageElement, const N: usize> StorageElement for [T; N] {
    const TAGGED: bool = T::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        for item in self {
            item.contribute_element(depth, out)?;
        }
        Ok(())
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        let mut items = Vec::with_capacity(N);
        for _ in 0..N {
            items.push(T::assemble_element(depth, cursor)?);
        }
        items.try_into().map_err(|_| StorageError::LeafBody(WireError::Length))
    }
}

impl<T: StorageElement> StorageElement for Option<T> {
    const TAGGED: bool = T::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        match self {
            None => encode_leaf(&0u8, out),
            Some(value) => {
                encode_leaf(&1u8, out)?;
                value.contribute_element(depth, out)
            }
        }
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        match decode_stream_leaf::<u8>(cursor)? {
            0 => Ok(None),
            1 => T::assemble_element(depth, cursor).map(Some),
            presence => Err(StorageError::LeafBody(WireError::InvalidBool(presence))),
        }
    }
}

impl<K: StorageElement + Ord, V: StorageElement> StorageElement for BTreeMap<K, V> {
    const TAGGED: bool = K::TAGGED || V::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        // Canonical map order is ascending encoded-key bytes, matching the
        // positional wire codec, so an all-positional map stays byte-identical.
        let mut entries = Vec::with_capacity(self.len());
        for (key, value) in self {
            let mut key_bytes = Vec::new();
            key.contribute_element(depth, &mut key_bytes)?;
            let mut value_bytes = Vec::new();
            value.contribute_element(depth, &mut value_bytes)?;
            entries.push((key_bytes, value_bytes));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        contribute_count(entries.len(), out)?;
        for (key_bytes, value_bytes) in entries {
            out.extend_from_slice(&key_bytes);
            out.extend_from_slice(&value_bytes);
        }
        Ok(())
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        let count = decode_stream_leaf::<u32>(cursor)? as usize;
        let mut map = Self::new();
        for _ in 0..count {
            let key = K::assemble_element(depth, cursor)?;
            let value = V::assemble_element(depth, cursor)?;
            map.insert(key, value);
        }
        Ok(map)
    }
}

fn contribute_count(len: usize, out: &mut Vec<u8>) -> Result<(), StorageError> {
    encode_leaf(&u32::try_from(len).map_err(|_| StorageError::LeafBody(WireError::Length))?, out)
}
