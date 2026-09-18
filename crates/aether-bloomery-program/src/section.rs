//! Const-assembled `aether.bloomery.programs` records and a `no_std` decoder.
//!
//! `declaration::<P>()` allocates and panics on a bad name, so it cannot run
//! in const context. Each generated record is instead a documented byte layout
//! assembled from the program `NAME` / `INTENT` literals and the input/result
//! [`KindId`]s. wasm-ld concatenates same-named custom sections, so the decoder
//! walks concatenated records:
//!
//! ```text
//! version:     u8  = 1
//! name_len:    u16 little-endian
//! name:        name_len UTF-8 bytes
//! input:       u64 little-endian KindId
//! result:      u64 little-endian KindId
//! mode:        u8  (0 = Pure, 1 = Sampled)
//! intent_len:  u16 little-endian
//! intent:      intent_len UTF-8 bytes
//! ```

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;
use core::str;

use aether_data::KindId;

use crate::kinds::{Mode, Program, ProgramName};

/// Record version byte written at the start of every program declaration.
pub const SECTION_VERSION: u8 = 1;
/// [`Mode::Pure`] discriminant in a declaration record.
pub const MODE_PURE: u8 = 0;
/// [`Mode::Sampled`] discriminant in a declaration record.
pub const MODE_SAMPLED: u8 = 1;

/// Byte length of one declaration record for `name` and `intent`.
#[must_use]
pub const fn program_record_len(name: &[u8], intent: &[u8]) -> usize {
    1 + 2 + name.len() + 8 + 8 + 1 + 2 + intent.len()
}

/// Const-assemble one version-prefixed declaration record.
///
/// # Panics
///
/// Panics when `N` is not [`program_record_len`] for the same `name` and
/// `intent`, or when either string is longer than `u16::MAX`.
#[must_use]
pub const fn write_program_record<const N: usize>(
    name: &[u8],
    input: u64,
    result: u64,
    mode: u8,
    intent: &[u8],
) -> [u8; N] {
    assert!(N == program_record_len(name, intent), "aether-bloomery-program: program record length mismatch");
    let mut out = [0u8; N];
    let mut pos = 0;
    out[pos] = SECTION_VERSION;
    pos += 1;
    write_u16_le(&mut out, &mut pos, u16_len(name));
    write_slice(&mut out, &mut pos, name);
    write_u64_le(&mut out, &mut pos, input);
    write_u64_le(&mut out, &mut pos, result);
    out[pos] = mode;
    pos += 1;
    write_u16_le(&mut out, &mut pos, u16_len(intent));
    write_slice(&mut out, &mut pos, intent);
    let _ = pos;
    out
}

#[allow(clippy::cast_possible_truncation)]
const fn u16_len(bytes: &[u8]) -> u16 {
    assert!(bytes.len() <= u16::MAX as usize, "aether-bloomery-program: program name or intent exceeds u16::MAX");
    bytes.len() as u16
}

const fn write_u16_le(out: &mut [u8], pos: &mut usize, value: u16) {
    let bytes = value.to_le_bytes();
    out[*pos] = bytes[0];
    out[*pos + 1] = bytes[1];
    *pos += 2;
}

const fn write_u64_le(out: &mut [u8], pos: &mut usize, value: u64) {
    let bytes = value.to_le_bytes();
    let mut index = 0;
    while index < 8 {
        out[*pos] = bytes[index];
        *pos += 1;
        index += 1;
    }
}

const fn write_slice(out: &mut [u8], pos: &mut usize, bytes: &[u8]) {
    let mut index = 0;
    while index < bytes.len() {
        out[*pos] = bytes[index];
        *pos += 1;
        index += 1;
    }
}

/// Why [`declarations`] refused a custom-section payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclarationsError {
    /// The remaining bytes were shorter than a record header or declared field.
    Truncated,
    /// The record version byte is not [`SECTION_VERSION`].
    UnsupportedVersion(u8),
    /// A name or intent field was not UTF-8.
    InvalidUtf8,
    /// The name field is not a valid [`ProgramName`].
    InvalidName,
    /// The mode byte is not [`MODE_PURE`] or [`MODE_SAMPLED`].
    UnknownMode(u8),
}

impl fmt::Display for DeclarationsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated program declaration record"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported program declaration version {version}"),
            Self::InvalidUtf8 => f.write_str("program declaration field is not UTF-8"),
            Self::InvalidName => f.write_str("program declaration name is not a ProgramName"),
            Self::UnknownMode(mode) => write!(f, "unknown program mode {mode}"),
        }
    }
}

impl StdError for DeclarationsError {}

/// Decode concatenated [`aether.bloomery.programs`] records.
///
/// # Errors
///
/// [`DeclarationsError`] when a record is truncated, versioned incorrectly,
/// or carries an invalid name, UTF-8 field, or mode.
pub fn declarations(section: &[u8]) -> Result<Vec<Program>, DeclarationsError> {
    let mut rest = section;
    let mut out = Vec::new();
    while !rest.is_empty() {
        out.push(read_record(&mut rest)?);
    }
    Ok(out)
}

fn read_record(rest: &mut &[u8]) -> Result<Program, DeclarationsError> {
    let version = read_u8(rest)?;
    if version != SECTION_VERSION {
        return Err(DeclarationsError::UnsupportedVersion(version));
    }
    let name = read_len_prefixed_string(rest)?;
    let input = KindId(read_u64(rest)?);
    let result = KindId(read_u64(rest)?);
    let mode = match read_u8(rest)? {
        MODE_PURE => Mode::Pure,
        MODE_SAMPLED => Mode::Sampled,
        mode => return Err(DeclarationsError::UnknownMode(mode)),
    };
    let intent = read_len_prefixed_string(rest)?;
    let name = ProgramName::new(name).map_err(|_| DeclarationsError::InvalidName)?;
    Ok(Program { name, input, result, mode, intent })
}

fn read_u8(rest: &mut &[u8]) -> Result<u8, DeclarationsError> {
    let (byte, tail) = rest.split_first().ok_or(DeclarationsError::Truncated)?;
    *rest = tail;
    Ok(*byte)
}

fn read_u16(rest: &mut &[u8]) -> Result<u16, DeclarationsError> {
    if rest.len() < 2 {
        return Err(DeclarationsError::Truncated);
    }
    let (head, tail) = rest.split_at(2);
    *rest = tail;
    Ok(u16::from_le_bytes([head[0], head[1]]))
}

fn read_u64(rest: &mut &[u8]) -> Result<u64, DeclarationsError> {
    if rest.len() < 8 {
        return Err(DeclarationsError::Truncated);
    }
    let (head, tail) = rest.split_at(8);
    *rest = tail;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(head);
    Ok(u64::from_le_bytes(bytes))
}

fn read_len_prefixed_string(rest: &mut &[u8]) -> Result<String, DeclarationsError> {
    let len = usize::from(read_u16(rest)?);
    if rest.len() < len {
        return Err(DeclarationsError::Truncated);
    }
    let (head, tail) = rest.split_at(len);
    *rest = tail;
    str::from_utf8(head).map(String::from).map_err(|_| DeclarationsError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use aether_data::KindId;

    use super::{MODE_PURE, declarations, program_record_len, write_program_record};
    use crate::kinds::Mode;

    #[test]
    fn concatenated_records_decode_name_ids_mode_and_intent() {
        const FIRST_NAME: &[u8] = b"test.program.one";
        const FIRST_INTENT: &[u8] = b"first";
        const SECOND_NAME: &[u8] = b"test.program.two";
        const SECOND_INTENT: &[u8] = b"second";
        const FIRST_LEN: usize = program_record_len(FIRST_NAME, FIRST_INTENT);
        const SECOND_LEN: usize = program_record_len(SECOND_NAME, SECOND_INTENT);
        let first = write_program_record::<FIRST_LEN>(FIRST_NAME, 1, 2, MODE_PURE, FIRST_INTENT);
        let second = write_program_record::<SECOND_LEN>(SECOND_NAME, 3, 4, MODE_PURE, SECOND_INTENT);
        let mut section = first.to_vec();
        section.extend_from_slice(&second);

        let decoded = declarations(&section).expect("records decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].name.as_str(), "test.program.one");
        assert_eq!(decoded[0].input, KindId(1));
        assert_eq!(decoded[0].result, KindId(2));
        assert_eq!(decoded[0].mode, Mode::Pure);
        assert_eq!(decoded[0].intent, "first");
        assert_eq!(decoded[1].name.as_str(), "test.program.two");
        assert_eq!(decoded[1].input, KindId(3));
        assert_eq!(decoded[1].result, KindId(4));
        assert_eq!(decoded[1].intent, "second");
    }
}
