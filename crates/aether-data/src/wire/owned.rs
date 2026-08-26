//! Owned encode/decode traits for the aether wire format (ADR-0118, ADR-0188).
//!
//! `#[derive(Schema)]` emits these from the same field list as `SCHEMA`, so a
//! schema change *is* a codec change. The lifetime on [`WireDecode`] keeps
//! ADR-0188 §4 reachable; every derived impl (and every leaf here) still
//! decodes owned — the serde `borrow` attribute has no consumer under `crates/`.

use alloc::vec::Vec;

use super::Error;

/// Append this value's ADR-0118 bytes to `out`.
pub trait WireEncode {
    /// Encode `self` onto the end of `out`.
    ///
    /// # Errors
    ///
    /// Fails only when a length exceeds the `u32` ceiling.
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error>;
}

/// Reconstruct `Self` from the front of a borrowed-slice cursor.
pub trait WireDecode<'de>: Sized {
    /// Pull one `Self` off the front of `cursor`, advancing it.
    ///
    /// # Errors
    ///
    /// Unexpected EOF, an invalid bool/presence/enum byte, a length past the
    /// remaining input, invalid UTF-8, or an out-of-range `char`.
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, Error>;
}

/// Encode a value to owned wire bytes through [`WireEncode`].
///
/// # Errors
///
/// Fails only when a length exceeds the `u32` ceiling.
pub fn encode_to_vec<T: WireEncode + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    value.encode(&mut out)?;
    Ok(out)
}

/// Decode a value from a wire payload through [`WireDecode`], requiring every
/// byte consumed.
///
/// # Errors
///
/// Decode faults, or [`Error::TrailingBytes`] when input remains.
pub fn decode_from_slice<'a, T: WireDecode<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    let mut cursor = bytes;
    let value = T::decode(&mut cursor)?;
    if cursor.is_empty() {
        Ok(value)
    } else {
        Err(Error::TrailingBytes)
    }
}

/// Decode a value from the front of a wire payload, returning the unconsumed
/// remainder.
///
/// # Errors
///
/// Decode faults from [`WireDecode`].
pub fn take_from_slice<'a, T: WireDecode<'a>>(bytes: &'a [u8]) -> Result<(T, &'a [u8]), Error> {
    let mut cursor = bytes;
    let value = T::decode(&mut cursor)?;
    Ok((value, cursor))
}

/// Split `n` bytes off the front of `cursor`.
///
/// # Errors
///
/// [`Error::UnexpectedEof`] when fewer than `n` bytes remain.
pub fn take<'de>(cursor: &mut &'de [u8], n: usize) -> Result<&'de [u8], Error> {
    if cursor.len() < n {
        return Err(Error::UnexpectedEof);
    }
    let (head, tail) = cursor.split_at(n);
    *cursor = tail;
    Ok(head)
}

/// Split a fixed-size array off the front of `cursor`.
///
/// # Errors
///
/// [`Error::UnexpectedEof`] when fewer than `N` bytes remain.
pub fn take_array<const N: usize>(cursor: &mut &[u8]) -> Result<[u8; N], Error> {
    let mut out = [0u8; N];
    out.copy_from_slice(take(cursor, N)?);
    Ok(out)
}

/// Write a `u32` little-endian count. Fails past the `u32` ceiling.
///
/// # Errors
///
/// [`Error::Length`] when `len` does not fit in `u32`.
pub fn write_count(out: &mut Vec<u8>, len: usize) -> Result<(), Error> {
    let count = u32::try_from(len).map_err(|_| Error::Length)?;
    out.extend_from_slice(&count.to_le_bytes());
    Ok(())
}

/// Bytes arm: `u32` little-endian count then the raw run (memcpy path).
///
/// # Errors
///
/// [`Error::Length`] when `bytes.len()` exceeds `u32`.
pub fn encode_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    write_count(out, bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

/// Bytes arm decode: `u32` count then that many raw bytes, copied owned.
///
/// # Errors
///
/// [`Error::UnexpectedEof`] when the count overruns the remaining input.
pub fn decode_bytes(cursor: &mut &[u8]) -> Result<Vec<u8>, Error> {
    let len = u32::from_le_bytes(take_array(cursor)?) as usize;
    Ok(take(cursor, len)?.to_vec())
}

/// Presence byte for `Option` / `bool`: `0` or `1`.
///
/// # Errors
///
/// [`Error::UnexpectedEof`] or [`Error::InvalidBool`].
pub fn read_presence(cursor: &mut &[u8]) -> Result<bool, Error> {
    match take(cursor, 1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(Error::InvalidBool(other)),
    }
}

/// Encode a sequence: `u32` count then elements in iteration order.
///
/// # Errors
///
/// [`Error::Length`] past the `u32` ceiling, or an element encode fault.
pub fn encode_seq<T: WireEncode>(out: &mut Vec<u8>, items: &[T]) -> Result<(), Error> {
    write_count(out, items.len())?;
    for item in items {
        item.encode(out)?;
    }
    Ok(())
}

/// Decode a sequence into an owned `Vec`.
///
/// # Errors
///
/// Count overrun or an element decode fault.
pub fn decode_seq<'de, T: WireDecode<'de>>(cursor: &mut &'de [u8]) -> Result<Vec<T>, Error> {
    let count = u32::from_le_bytes(take_array(cursor)?) as usize;
    let mut items = Vec::with_capacity(count.min(cursor.len()));
    for _ in 0..count {
        items.push(T::decode(cursor)?);
    }
    Ok(items)
}
