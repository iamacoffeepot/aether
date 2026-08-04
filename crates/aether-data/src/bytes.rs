//! Serde routing for `Bytes`-typed fields: `#[serde(with = "aether_data::bytes")]`.
//!
//! `Vec<u8>`'s own `Serialize` impl walks `serialize_seq`, one element at a
//! time, so the wire serializer's `serialize_bytes` fast path (ADR-0118) is
//! never reached and a large payload pays per-byte dispatch and buffering.
//! This module reroutes a field to `serialize_bytes` / `deserialize_byte_buf`
//! — memcpy semantics on both sides of the mail codec.
//!
//! The wire bytes are identical either way: a seq of `u8` encodes as the
//! `u32` little-endian count then one raw byte per element, which is
//! byte-for-byte the `serialize_bytes` layout (count then the raw run). The
//! attribute is therefore a pure throughput change — old and new binaries
//! agree on every payload — and a field that omits it stays correct, just
//! slow. The deserialize side also accepts a seq of integers so
//! self-describing formats that render bytes as an array (`serde_json`)
//! keep decoding exactly as the plain `Vec<u8>` impl did.

use alloc::vec::Vec;
use core::fmt;

use serde::de::{Error as DeError, SeqAccess, Visitor};
use serde::{Deserializer, Serializer};

/// Serialize a byte buffer through the serializer's `serialize_bytes` fast
/// path instead of `Vec<u8>`'s per-element seq walk.
pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bytes(bytes)
}

/// Deserialize a byte buffer through `deserialize_byte_buf`, accepting the
/// borrowed/owned bytes a binary format hands over or the integer seq a
/// self-describing format falls back to.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    deserializer.deserialize_byte_buf(BytesVisitor)
}

struct BytesVisitor;

impl<'de> Visitor<'de> for BytesVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a byte buffer")
    }

    fn visit_bytes<E: DeError>(self, v: &[u8]) -> Result<Vec<u8>, E> {
        Ok(v.to_vec())
    }

    fn visit_byte_buf<E: DeError>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
        Ok(v)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(byte) = seq.next_element::<u8>()? {
            out.push(byte);
        }
        Ok(out)
    }
}
