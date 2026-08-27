//! Leaf-body encode and decode.
//!
//! This module is the only place in the storage tree that names the
//! ADR-0188 owned codec ([`crate::wire::WireEncode`] / [`crate::wire::WireDecode`]).
//! The TLV layer contributes the field-hash-and-length envelope; leaf
//! bodies are the same bytes the positional wire already uses for that
//! type. Binding to a later name for those traits is an edit to this
//! file and nothing else.

use alloc::vec::Vec;

use super::record::StorageError;
use crate::wire::{WireDecode, WireEncode, decode_from_slice};

/// Marker for types whose values can be a TLV leaf body. Implemented
/// for every [`WireEncode`] + [`WireDecode`] pair so the rest of the
/// storage tree never names those traits.
pub trait LeafBody: WireEncode + for<'de> WireDecode<'de> {}

impl<T: WireEncode + for<'de> WireDecode<'de>> LeafBody for T {}

/// Encode `value` as an ADR-0118 leaf body onto `out`.
pub fn encode_leaf<T: LeafBody>(value: &T, out: &mut Vec<u8>) -> Result<(), StorageError> {
    value.encode(out).map_err(StorageError::from)
}

/// Decode one leaf body, requiring every byte consumed.
pub fn decode_leaf<T: LeafBody>(bytes: &[u8]) -> Result<T, StorageError> {
    decode_from_slice(bytes).map_err(StorageError::from)
}
