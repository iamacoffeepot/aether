//! The aether wire format (ADR-0118) — the owned, schema-driven, fixed-width
//! encoding for structured (non-cast) kinds.
//!
//! The format is defined here. `serde` is a *consumer* that drives it: the
//! `ser` adapter walks a Rust value through serde and emits the bytes, and the
//! `de` adapter reads them back. The schema-driven JSON walker (in
//! `aether-codec`, step 2 of the ADR-0118 arc) is the other consumer over the
//! same byte layout; the two must emit identical bytes for the same value,
//! which is why the encoding carries no type or field tags — both ends already
//! know the shape (the Rust type, or the `SchemaType`).
//!
//! Encoding rules (ADR-0118 §The format):
//! - little-endian, fixed-width scalars (the declared width; no variable-length
//!   integers, no zigzag), bit-faithful floats;
//! - `bool` and option-presence are one byte (`0` / `1`);
//! - `String` / `Bytes` / `Vec` / `Map` are a `u32` little-endian count, then
//!   the elements (maps in ascending encoded-key byte order — canonical);
//! - struct / tuple / array fields are positional, no names, no count;
//! - sum-type selectors (`Enum`, `Ref`) are a fixed `u32` (serde's
//!   `variant_index`), then the selected variant's body.
//!
//! Nothing in the workspace encodes through this module yet; ADR-0118 step 2
//! repoints the derive runtime and the schema walker onto it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use serde::de::Error as DeError;
use serde::ser::Error as SerError;
use serde::{Deserialize, Serialize};

mod de;
mod ser;

#[cfg(test)]
mod tests;

/// A wire encode or decode failure. Encoding fails only when a length exceeds
/// the `u32` ceiling; everything else is a decode-side fault.
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
            Self::NotSelfDescribing => {
                f.write_str("aether wire: format is not self-describing (deserialize_any)")
            }
            Self::Message(m) => f.write_str(m),
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
