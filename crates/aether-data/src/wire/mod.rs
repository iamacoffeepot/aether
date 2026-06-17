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

/// Format-version byte prefixing every top-level payload (ADR-0118 §Envelope).
/// It versions the *encoding*, independent of `KindId` which versions the
/// *schema*. Nested values carry no version byte — only the top-level
/// [`to_vec`] / [`from_bytes`] / [`take_from_bytes`] boundary does.
pub const WIRE_VERSION: u8 = 1;

/// A wire encode or decode failure. Encoding fails only when a length exceeds
/// the `u32` ceiling; everything else is a decode-side fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Input ended mid-value.
    UnexpectedEof,
    /// A `bool` or option-presence byte was neither `0` nor `1`.
    InvalidBool(u8),
    /// The top-level payload did not begin with [`WIRE_VERSION`].
    BadVersion(u8),
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
            Self::BadVersion(v) => write!(f, "aether wire: bad format version byte {v}"),
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

/// Encode a value to a versioned wire payload: [`WIRE_VERSION`] followed by the
/// value's encoding.
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let body = to_vec_bare(value)?;
    let mut out = Vec::with_capacity(body.len() + 1);
    out.push(WIRE_VERSION);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Encode a value to **bare** wire bytes with no leading [`WIRE_VERSION`]. The
/// bytes are an interior value, not a self-describing kind image — for
/// hand-assembly sites (e.g. the DAG executor) that concatenate interior values
/// into one kind image and prepend the single version byte themselves. Most
/// callers want [`to_vec`].
pub fn to_vec_bare<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut serializer = ser::Serializer::new();
    value.serialize(&mut serializer)?;
    Ok(serializer.into_output())
}

/// Decode a value from a versioned wire payload, requiring every byte consumed.
pub fn from_bytes<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    let body = strip_version(bytes)?;
    let mut deserializer = de::Deserializer::new(body);
    let value = T::deserialize(&mut deserializer)?;
    if deserializer.is_empty() {
        Ok(value)
    } else {
        Err(Error::TrailingBytes)
    }
}

/// Decode a value from the front of a versioned wire payload, returning the
/// value and the unconsumed remainder (after the value, not the version byte).
pub fn take_from_bytes<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<(T, &'a [u8]), Error> {
    let body = strip_version(bytes)?;
    let mut deserializer = de::Deserializer::new(body);
    let value = T::deserialize(&mut deserializer)?;
    Ok((value, deserializer.remaining()))
}

/// Decode a value from **bare** wire bytes carrying no leading [`WIRE_VERSION`],
/// requiring every byte consumed. The symmetric counterpart to [`to_vec_bare`]:
/// for sites that frame the bare structural body themselves and version the
/// frame independently of [`WIRE_VERSION`] — e.g. the `aether.kinds` manifest,
/// whose per-section version byte versions the encoding and whose bare body is
/// the `Kind::ID` hash input. Most callers want [`from_bytes`].
pub fn from_bytes_bare<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    let mut deserializer = de::Deserializer::new(bytes);
    let value = T::deserialize(&mut deserializer)?;
    if deserializer.is_empty() {
        Ok(value)
    } else {
        Err(Error::TrailingBytes)
    }
}

/// Decode a value from the front of **bare** wire bytes (no leading
/// [`WIRE_VERSION`]), returning the value and the unconsumed remainder. The
/// symmetric counterpart to [`to_vec_bare`] for walking back-to-back bare
/// records (e.g. the manifest reader stepping `[version][bare body]` records,
/// where the version byte is the frame's, not [`WIRE_VERSION`]).
pub fn take_from_bytes_bare<'a, T: Deserialize<'a>>(
    bytes: &'a [u8],
) -> Result<(T, &'a [u8]), Error> {
    let mut deserializer = de::Deserializer::new(bytes);
    let value = T::deserialize(&mut deserializer)?;
    Ok((value, deserializer.remaining()))
}

fn strip_version(bytes: &[u8]) -> Result<&[u8], Error> {
    match bytes.split_first() {
        Some((&v, rest)) if v == WIRE_VERSION => Ok(rest),
        Some((&v, _)) => Err(Error::BadVersion(v)),
        None => Err(Error::UnexpectedEof),
    }
}
