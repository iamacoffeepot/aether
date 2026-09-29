//! The aether wire format (ADR-0118) — the owned, schema-driven, fixed-width
//! encoding for structured (non-cast) kinds.
//!
//! [`WireEncode`] / [`WireDecode`] are the typed codec. `#[derive(Schema)]`
//! emits them from the same field list as `SCHEMA` (ADR-0188), so a schema
//! change is a codec change. The schema-driven JSON walker in `aether-codec`
//! (`encode_schema` / `decode_schema`) walks the same byte layout from a
//! `SchemaType` with no Rust type in sight. The serde `ser` / `de` adapters
//! remain for protocol frames that are not `Schema` types (`WireFrame`,
//! `PlayerFrame`, the fuzz corpus); they must emit identical bytes for the
//! same value, which is why the encoding carries no type or field tags.
//!
//! Encoding rules (ADR-0118 §The format):
//! - little-endian, fixed-width scalars (the declared width; no variable-length
//!   integers, no zigzag), bit-faithful floats;
//! - `bool` and option-presence are one byte (`0` / `1`);
//! - `String` / `Bytes` / `Vec` / `Map` are a `u32` little-endian count, then
//!   the elements (maps in ascending encoded-key byte order — canonical);
//! - struct / tuple / array fields are positional, no names, no count;
//! - sum-type selectors (`Enum`, `Ref`) are a fixed `u32` (serde's
//!   `variant_index`), then the selected variant's body;
//! - a `Blob` is a one-byte tag (ADR-0238): tag 0 is a `u32` count then the
//!   bytes, tag 1 the blob's 32-byte hash, written only by the in-process
//!   envelope encoder through the [`Encoder`] hook;
//! - a held-reply ticket (ADR-0243) is a `u64`, written only through
//!   [`Encoder::held`] on a [`LedgerEncoder`] and claimed back only through
//!   [`Decoder::claim_held`] on a [`DecodeCtx`] granted a [`HeldLedger`].
//!
//! A decode that needs the engine reaches it only through a [`DecodeCtx`]:
//! a tag-1 `Blob` resolves through [`Decoder::resolve_blob`], a
//! `ProtocolPath` proves its route through [`Decoder::prove_route_covers`],
//! and a held ticket is claimed through [`Decoder::claim_held`]. The plain
//! `&[u8]` decoder behind [`decode_from_slice`] refuses all three, as
//! [`DecodeCtx::empty`] does.
//!
//! This module is the workspace's structured wire format (ADR-0118,
//! shipped). Kind encode/decode funnels through [`WireEncode`] /
//! [`WireDecode`]. The serde adapter still backs `to_vec` / `from_bytes`
//! for non-Schema protocol frames. The external `postcard` crate it
//! replaced is gone — no crate in the workspace depends on it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use serde::de::Error as DeError;
use serde::ser::Error as SerError;
use serde::{Deserialize, Serialize};

use crate::blob::BlobHash;
use crate::{ErasedActorPath, KindId};

mod attach;
mod context;
mod de;
mod leaf;
pub(crate) mod owned;
mod ser;
mod vocabulary;

#[cfg(test)]
mod differential;
#[cfg(test)]
mod tests;

pub(crate) use attach::InCtx;
pub use attach::{BlobResolver, Decoder, Encoder, HeldClaim, HeldLedger, LedgerEncoder};
pub use context::{DecodeCtx, PublishedRoutes};
pub use owned::{
    WireDecode, WireEncode, decode_bytes, decode_from_slice, encode_bytes, encode_to_vec, take_from_slice,
};

/// A wire encode or decode failure. Encoding fails only when a length exceeds
/// the `u32` ceiling or a held ticket reaches an encoder that grants none;
/// everything else is a decode-side fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Input ended mid-value.
    UnexpectedEof,
    /// A `bool` or option-presence byte was neither `0` nor `1`.
    InvalidBool(u8),
    /// A length or count exceeded the `u32` ceiling on encode, or the remaining
    /// input on decode.
    Length,
    /// String bytes were not valid UTF-8.
    Utf8,
    /// A `char` code point was out of range.
    InvalidChar(u32),
    /// [`from_bytes`] left input unconsumed.
    TrailingBytes,
    /// A self-describing operation the format cannot serve (`deserialize_any`).
    NotSelfDescribing,
    /// A `serde` `custom` message.
    Message(String),
    /// An enum selector that does not name a declared variant.
    InvalidEnum(u32),
    /// A decoded load name that breaks the segment grammar.
    InvalidLoadName,
    /// A decoded actor path that breaks the ADR-0166 address grammar.
    InvalidActorPath,
    /// A `Blob` field's tag was neither `0` (inline bytes) nor `1` (a hash).
    InvalidBlobTag(u8),
    /// A tag-1 `Blob` field whose hash the decode's resolver does not supply.
    DetachedBlob(BlobHash),
    /// A `ProtocolPath` decoded by a context with no published routes.
    ProtocolPathUnchecked { path: ErasedActorPath },
    /// A `ProtocolPath` whose path no route has stood under, or whose route
    /// is still starting and has published no contract.
    ProtocolPathUnpublished { path: ErasedActorPath },
    /// A `ProtocolPath` whose route does not publish `kind`'s row, or
    /// publishes it with another reply.
    UncoveredProtocolPath { path: ErasedActorPath, kind: KindId },
    /// A hand-written kind with no decode body.
    NoDecodeBody { kind: &'static str },
    /// A held ticket answering `reply` reached an encoder or decode context
    /// that grants no [`HeldLedger`] (ADR-0243).
    HeldUngranted { reply: KindId },
    /// A granted [`HeldLedger`] does not accept `ticket` as an obligation to
    /// answer `reply` (ADR-0243).
    HeldUnclaimed { ticket: u64, reply: KindId },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => f.write_str("aether wire: unexpected end of input"),
            Self::InvalidBool(b) => write!(f, "aether wire: invalid bool/presence byte {b}"),
            Self::Length => f.write_str("aether wire: length exceeds the u32 ceiling"),
            Self::Utf8 => f.write_str("aether wire: string is not valid UTF-8"),
            Self::InvalidChar(c) => write!(f, "aether wire: invalid char code point {c}"),
            Self::TrailingBytes => f.write_str("aether wire: trailing bytes after value"),
            Self::NotSelfDescribing => f.write_str("aether wire: format is not self-describing (deserialize_any)"),
            Self::Message(m) => f.write_str(m),
            Self::InvalidEnum(selector) => write!(f, "aether wire: invalid enum selector {selector}"),
            Self::InvalidLoadName => f.write_str("aether wire: invalid load name"),
            Self::InvalidActorPath => f.write_str("aether wire: invalid actor path"),
            Self::InvalidBlobTag(tag) => write!(f, "aether wire: invalid blob tag {tag}"),
            Self::DetachedBlob(_) => f.write_str("aether wire: blob hash not supplied by the decode's resolver"),
            Self::ProtocolPathUnchecked { path } => {
                write!(f, "aether wire: protocol path `{path}` refused: this context has no registry")
            }
            Self::ProtocolPathUnpublished { path } => {
                write!(f, "aether wire: protocol path `{path}` refused: no route has published at this path")
            }
            Self::UncoveredProtocolPath { path, kind } => {
                write!(f, "aether wire: protocol path `{path}` refused: `{kind}`'s row is missing or different")
            }
            Self::NoDecodeBody { kind } => write!(f, "aether wire: kind `{kind}` has no decode body"),
            Self::HeldUngranted { reply } => {
                write!(f, "aether wire: held ticket answering `{reply}` refused: no ledger is granted here")
            }
            Self::HeldUnclaimed { ticket, reply } => {
                write!(f, "aether wire: held ticket {ticket} answering `{reply}` is not in the granted ledger")
            }
        }
    }
}

impl StdError for Error {}

impl SerError for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::Message(msg.to_string())
    }
}

impl DeError for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::Message(msg.to_string())
    }
}

/// Encode a value to wire bytes. The encoding is unversioned: format agreement
/// is the transport's job (the RPC handshake negotiates a `wire_version` between
/// binaries) and is compile-time-fixed within one binary, so the bytes carry no
/// per-payload version (ADR-0118 §Envelope).
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut serializer = ser::Serializer::new();
    value.serialize(&mut serializer)?;
    Ok(serializer.into_output())
}

/// Decode a value from a wire payload, requiring every byte consumed.
pub fn from_bytes<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    let mut deserializer = de::Deserializer::new(bytes);
    let value = T::deserialize(&mut deserializer)?;
    if deserializer.is_empty() {
        Ok(value)
    } else {
        Err(Error::TrailingBytes)
    }
}

/// Decode a value from the front of a wire payload, returning the value and the
/// unconsumed remainder — for walking back-to-back records in one buffer.
pub fn take_from_bytes<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<(T, &'a [u8]), Error> {
    let mut deserializer = de::Deserializer::new(bytes);
    let value = T::deserialize(&mut deserializer)?;
    Ok((value, deserializer.remaining()))
}
