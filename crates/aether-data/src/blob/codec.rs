//! `Blob` as a schema leaf (ADR-0238 decisions 3 and 5).
//!
//! The binary form is a one-byte tag. Tag 0 is a `u32` little-endian length
//! and the bytes: every codec writes it and every decode source reads it.
//! Tag 1 is the blob's 32-byte hash, which only the in-process envelope
//! encoder writes through [`Encoder::blob`]. A decode hands a tag-1 hash to
//! [`Decoder::resolve`], which refuses unless the decode was given a
//! resolver. The serde impls write the tag-0 form too, so `wire::to_vec`
//! and the [`WireEncode`] path agree byte for byte.

use alloc::vec::Vec;
use core::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::ser::{Error as _, SerializeTuple};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{Blob, BlobHash, BlobReader, Repr};
use crate::schema::{LabelNode, SchemaType};
use crate::wire::owned::{decode_bytes, take, take_array, write_count};
use crate::wire::{Decoder, Encoder, Error, WireDecode, WireEncode};
use crate::{CastEligible, Schema};

/// Inline bytes: a `u32` length, then the bytes.
const TAG_INLINE: u8 = 0;
/// A shared entry's hash: written only in in-process mail.
const TAG_HASH: u8 = 1;

impl Schema for Blob {
    const SCHEMA: SchemaType = SchemaType::Blob;
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl CastEligible for Blob {
    const ELIGIBLE: bool = false;
}

/// Write `value` as tag 0: the tag, a `u32` length, then the bytes.
pub fn encode_inline(out: &mut Vec<u8>, value: &Blob) -> Result<(), Error> {
    let reader = BlobReader::open(value);
    let len = usize::try_from(reader.len()).map_err(|_| Error::Length)?;
    out.push(TAG_INLINE);
    write_count(out, len)?;
    append_bytes(out, &reader, len)
}

/// Append the `len` bytes behind `reader` to `out`, streamed so a `Shared`
/// value is copied once, straight into `out`.
fn append_bytes(out: &mut Vec<u8>, reader: &BlobReader<'_>, len: usize) -> Result<(), Error> {
    let start = out.len();
    out.resize(start + len, 0);
    let mut filled = 0;
    while filled < len {
        let copied = reader.read_range(filled as u64, &mut out[start + filled..]);
        if copied == 0 {
            return Err(Error::UnexpectedEof);
        }
        filled += copied;
    }
    Ok(())
}

impl WireEncode for Blob {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.encode_to(out)
    }

    fn encode_to<E: Encoder + ?Sized>(&self, enc: &mut E) -> Result<(), Error> {
        enc.blob(self)
    }
}

impl<'de> WireDecode<'de> for Blob {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error> {
        Self::decode_from(cursor)
    }

    fn decode_from<D: Decoder<'de> + ?Sized>(dec: &mut D) -> Result<Self, Error> {
        let cursor = dec.cursor();
        match take(cursor, 1)?[0] {
            TAG_INLINE => decode_bytes(cursor).map(Self::from),
            TAG_HASH => {
                let hash = BlobHash::from_bytes(take_array(cursor)?);
                dec.resolve(hash)
            }
            other => Err(Error::InvalidBlobTag(other)),
        }
    }
}

/// A byte run serde writes with `serialize_bytes`: the wire serializer's
/// `u32` count then the raw bytes, which is the tag-0 body.
struct RawBytes<'a>(&'a [u8]);

impl Serialize for RawBytes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

impl Serialize for Blob {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut tuple = serializer.serialize_tuple(2)?;
        tuple.serialize_element(&TAG_INLINE)?;
        match &self.0 {
            Repr::Owned(bytes) => tuple.serialize_element(&RawBytes(bytes))?,
            Repr::Shared(_) => {
                let reader = BlobReader::open(self);
                let len = usize::try_from(reader.len()).map_err(|_| S::Error::custom(Error::Length))?;
                let mut bytes = Vec::with_capacity(len);
                append_bytes(&mut bytes, &reader, len).map_err(S::Error::custom)?;
                tuple.serialize_element(&RawBytes(&bytes))?;
            }
        }
        tuple.end()
    }
}

/// An owned byte buffer from any of serde's byte shapes: the wire
/// deserializer's borrowed bytes, or a sequence of `u8` from a
/// self-describing format.
struct ByteBuf(Vec<u8>);

impl<'de> Deserialize<'de> for ByteBuf {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_byte_buf(ByteBufVisitor)
    }
}

struct ByteBufVisitor;

impl<'de> Visitor<'de> for ByteBufVisitor {
    type Value = ByteBuf;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("blob bytes")
    }

    fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<ByteBuf, E> {
        Ok(ByteBuf(bytes.to_vec()))
    }

    fn visit_byte_buf<E: de::Error>(self, bytes: Vec<u8>) -> Result<ByteBuf, E> {
        Ok(ByteBuf(bytes))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ByteBuf, A::Error> {
        let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
        while let Some(byte) = seq.next_element::<u8>()? {
            bytes.push(byte);
        }
        Ok(ByteBuf(bytes))
    }
}

struct BlobVisitor;

impl<'de> Visitor<'de> for BlobVisitor {
    type Value = Blob;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a tag-0 blob: the tag, then its bytes")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Blob, A::Error> {
        let tag: u8 = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(0, &self))?;
        if tag != TAG_INLINE {
            return Err(de::Error::custom(Error::InvalidBlobTag(tag)));
        }
        let ByteBuf(bytes) = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(1, &self))?;
        Ok(Blob::from(bytes))
    }
}

impl<'de> Deserialize<'de> for Blob {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_tuple(2, BlobVisitor)
    }
}
