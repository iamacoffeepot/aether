//! Const-assembled `aether.bloomery.programs` records and a `no_std` decoder.
//!
//! Each generated record is a documented const-assembled byte layout built
//! from the program `NAME` / `INTENT` / `DOC` literals, the input/result
//! [`KindId`]s, and the input's doc tree. wasm-ld concatenates same-named custom sections, so the decoder
//! walks concatenated records:
//!
//! ```text
//! version:     u8  = 3
//! name_len:    u16 little-endian
//! name:        name_len UTF-8 bytes
//! input:       u64 little-endian KindId
//! result:      u64 little-endian KindId
//! mode:        u8  (0 = Pure, 1 = Sampled)
//! apis:        u8  bit set (bit 0 = Http, bit 1 = Process, bit 2 = Workspace)
//! intent_len:  u16 little-endian
//! intent:      intent_len UTF-8 bytes
//! doc_len:     u32 little-endian
//! doc:         doc_len UTF-8 bytes
//! input_docs:  the input's DocNode as aether-wire bytes (self-delimiting)
//! ```
//!
//! The doc and the input's doc tree never enter a kind id: they describe the
//! program as a tool, beside the schema its input kind is hashed from.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;
use core::str;

use aether_data::canonical::{canonical_len_docs, canonical_write_docs};
use aether_data::{DocNode, KindId, wire};

use crate::kinds::{Mode, Program, ProgramApi, ProgramName};

/// Record version byte written at the start of every program declaration.
pub const SECTION_VERSION: u8 = 3;
/// [`Mode::Pure`] discriminant in a declaration record.
pub const MODE_PURE: u8 = 0;
/// [`Mode::Sampled`] discriminant in a declaration record.
pub const MODE_SAMPLED: u8 = 1;

/// Bit `i` of a record's `apis` byte names `API_BITS[i]`; [`api_mask`] sets
/// the same bits.
const API_BITS: [ProgramApi; 3] = [ProgramApi::Http, ProgramApi::Process, ProgramApi::Workspace];

/// The `apis` byte for a program whose `run` binds `apis`.
#[must_use]
pub const fn api_mask(apis: &[ProgramApi]) -> u8 {
    let mut mask = 0u8;
    let mut index = 0;
    while index < apis.len() {
        mask |= match apis[index] {
            ProgramApi::Http => 1,
            ProgramApi::Process => 1 << 1,
            ProgramApi::Workspace => 1 << 2,
        };
        index += 1;
    }
    mask
}

/// One decoded program record: the stored declaration plus the APIs its
/// `run` binds, which the driver checks against the providers its unit holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The stored declaration.
    pub program: Program,
    /// The APIs the program's `run` binds, in [`ProgramApi`] order.
    pub apis: Vec<ProgramApi>,
    /// The program's tool description, its `#[program]` impl doc.
    pub doc: String,
    /// The doc tree of the program's input: a doc for every field and
    /// variant it exposes.
    pub input_docs: DocNode,
}

/// One program's declaration record, as the bundle generator assembles it
/// at compile time.
#[derive(Clone, Copy)]
pub struct ProgramRecord<'a> {
    /// The program's `NAME`.
    pub name: &'a [u8],
    /// The input kind id.
    pub input: u64,
    /// The result kind id.
    pub result: u64,
    /// [`MODE_PURE`] or [`MODE_SAMPLED`].
    pub mode: u8,
    /// The [`api_mask`] of the APIs `run` binds.
    pub apis: u8,
    /// The program's `INTENT`.
    pub intent: &'a [u8],
    /// The program's `DOC`.
    pub doc: &'a [u8],
    /// The input's doc tree.
    pub input_docs: &'a DocNode,
}

/// Byte length of `record`.
#[must_use]
pub const fn program_record_len(record: &ProgramRecord<'_>) -> usize {
    1 + 2
        + record.name.len()
        + 8
        + 8
        + 1
        + 1
        + 2
        + record.intent.len()
        + 4
        + record.doc.len()
        + canonical_len_docs(record.input_docs)
}

/// Const-assemble one version-prefixed declaration record.
///
/// # Panics
///
/// Panics when `N` is not [`program_record_len`] for `record`, when its name
/// or intent is longer than `u16::MAX`, or when its input doc tree holds an
/// undocumented field.
#[must_use]
pub const fn write_program_record<const N: usize>(record: &ProgramRecord<'_>) -> [u8; N] {
    assert!(N == program_record_len(record), "aether-bloomery-program: program record length mismatch");
    let mut out = [0u8; N];
    let mut pos = 0;
    out[pos] = SECTION_VERSION;
    pos += 1;
    write_u16_le(&mut out, &mut pos, u16_len(record.name));
    write_slice(&mut out, &mut pos, record.name);
    write_u64_le(&mut out, &mut pos, record.input);
    write_u64_le(&mut out, &mut pos, record.result);
    out[pos] = record.mode;
    pos += 1;
    out[pos] = record.apis;
    pos += 1;
    write_u16_le(&mut out, &mut pos, u16_len(record.intent));
    write_slice(&mut out, &mut pos, record.intent);
    write_u32_le(&mut out, &mut pos, u32_len(record.doc));
    write_slice(&mut out, &mut pos, record.doc);
    let _ = canonical_write_docs(record.input_docs, &mut out, pos);
    out
}

const fn u16_len(bytes: &[u8]) -> u16 {
    let mut len = 0u16;
    let mut index = 0;
    while index < bytes.len() {
        len = match len.checked_add(1) {
            Some(next) => next,
            None => panic!("aether-bloomery-program: program name or intent exceeds u16::MAX"),
        };
        index += 1;
    }
    len
}

const fn u32_len(bytes: &[u8]) -> u32 {
    assert!(bytes.len() <= u32::MAX as usize, "aether-bloomery-program: program doc exceeds u32::MAX");
    // The low four little-endian bytes of a length that fits.
    let wide = bytes.len().to_le_bytes();
    u32::from_le_bytes([wide[0], wide[1], wide[2], wide[3]])
}

const fn write_u32_le(out: &mut [u8], pos: &mut usize, value: u32) {
    let bytes = value.to_le_bytes();
    let mut index = 0;
    while index < 4 {
        out[*pos] = bytes[index];
        *pos += 1;
        index += 1;
    }
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
    /// The record version byte is not the current version, 3.
    UnsupportedVersion(u8),
    /// A name, intent, or doc field was not UTF-8.
    InvalidUtf8,
    /// The name field is not a valid [`ProgramName`].
    InvalidName,
    /// The mode byte is not 0 (`Pure`) or 1 (`Sampled`).
    UnknownMode(u8),
    /// The APIs byte sets a bit that names no [`ProgramApi`].
    UnknownApi(u8),
    /// Two records share a program name.
    DuplicateName(ProgramName),
    /// The input doc tree did not decode.
    InvalidDocs,
}

impl fmt::Display for DeclarationsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated program declaration record"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported program declaration version {version}"),
            Self::InvalidUtf8 => f.write_str("program declaration field is not UTF-8"),
            Self::InvalidName => f.write_str("program declaration name is not a ProgramName"),
            Self::UnknownMode(mode) => write!(f, "unknown program mode {mode}"),
            Self::UnknownApi(apis) => write!(f, "unknown program api bits {apis:#010b}"),
            Self::DuplicateName(name) => write!(f, "program declaration repeats program {}", name.as_str()),
            Self::InvalidDocs => f.write_str("program declaration input doc tree does not decode"),
        }
    }
}

impl StdError for DeclarationsError {}

/// Decode concatenated `aether.bloomery.programs` custom-section records.
///
/// # Errors
///
/// [`DeclarationsError`] when a record is truncated, versioned incorrectly,
/// or carries an invalid name, UTF-8 field, mode, API bit, or doc tree, or when two
/// records share a program name.
pub fn declarations(section: &[u8]) -> Result<Vec<Declaration>, DeclarationsError> {
    let mut rest = section;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let declaration = read_record(&mut rest)?;
        if out.iter().any(|existing: &Declaration| existing.program.name == declaration.program.name) {
            return Err(DeclarationsError::DuplicateName(declaration.program.name));
        }
        out.push(declaration);
    }
    Ok(out)
}

fn read_record(rest: &mut &[u8]) -> Result<Declaration, DeclarationsError> {
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
    let apis = read_apis(read_u8(rest)?)?;
    let intent = read_len_prefixed_string(rest)?;
    let doc_len = usize::try_from(read_u32(rest)?).map_err(|_| DeclarationsError::Truncated)?;
    let doc = read_string(rest, doc_len)?;
    let (input_docs, tail) = wire::take_from_bytes::<DocNode>(rest).map_err(|_| DeclarationsError::InvalidDocs)?;
    *rest = tail;
    let name = ProgramName::new(name).map_err(|_| DeclarationsError::InvalidName)?;
    Ok(Declaration { program: Program { name, input, result, mode, intent }, apis, doc, input_docs })
}

fn read_apis(mask: u8) -> Result<Vec<ProgramApi>, DeclarationsError> {
    if mask >> API_BITS.len() != 0 {
        return Err(DeclarationsError::UnknownApi(mask));
    }
    Ok(API_BITS.iter().enumerate().filter(|(bit, _)| mask & (1 << bit) != 0).map(|(_, api)| *api).collect())
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

fn read_u32(rest: &mut &[u8]) -> Result<u32, DeclarationsError> {
    if rest.len() < 4 {
        return Err(DeclarationsError::Truncated);
    }
    let (head, tail) = rest.split_at(4);
    *rest = tail;
    Ok(u32::from_le_bytes([head[0], head[1], head[2], head[3]]))
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
    read_string(rest, len)
}

fn read_string(rest: &mut &[u8], len: usize) -> Result<String, DeclarationsError> {
    if rest.len() < len {
        return Err(DeclarationsError::Truncated);
    }
    let (head, tail) = rest.split_at(len);
    *rest = tail;
    str::from_utf8(head).map(String::from).map_err(|_| DeclarationsError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use alloc::borrow::Cow;

    use aether_data::{Doc, DocCell, DocNode, FieldDoc, KindId};

    use super::{
        DeclarationsError, MODE_PURE, ProgramRecord, api_mask, declarations, program_record_len, write_program_record,
    };
    use crate::kinds::{Mode, ProgramApi, ProgramName};

    static LEAF: DocNode = DocNode::Leaf;
    static FIRST_DOCS: DocNode = DocNode::Struct {
        fields: Cow::Borrowed(&[FieldDoc {
            doc: Doc::Written(Cow::Borrowed("The text to read.")),
            node: DocCell::Static(&LEAF),
            opaque: "",
        }]),
    };

    /// A pure record.
    const fn record(
        name: &'static [u8],
        input: u64,
        result: u64,
        apis: u8,
        intent: &'static [u8],
        doc: &'static [u8],
        input_docs: &'static DocNode,
    ) -> ProgramRecord<'static> {
        ProgramRecord { name, input, result, mode: MODE_PURE, apis, intent, doc, input_docs }
    }

    #[test]
    fn concatenated_records_decode_name_ids_mode_intent_and_docs() {
        // Catches a v3 record whose doc bytes or doc tree shift the next record.
        const FIRST: ProgramRecord<'static> = record(b"test.program.one", 1, 2, 0, b"first", b"Do first.", &FIRST_DOCS);
        const SECOND: ProgramRecord<'static> = record(
            b"test.program.two",
            3,
            4,
            api_mask(&[ProgramApi::Http, ProgramApi::Workspace]),
            b"second",
            b"Do second.",
            &LEAF,
        );
        const FIRST_LEN: usize = program_record_len(&FIRST);
        const SECOND_LEN: usize = program_record_len(&SECOND);
        let first = write_program_record::<FIRST_LEN>(&FIRST);
        let second = write_program_record::<SECOND_LEN>(&SECOND);
        let mut section = first.to_vec();
        section.extend_from_slice(&second);

        let decoded = declarations(&section).expect("records decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].program.name.as_str(), "test.program.one");
        assert_eq!(decoded[0].program.input, KindId(1));
        assert_eq!(decoded[0].program.result, KindId(2));
        assert_eq!(decoded[0].program.mode, Mode::Pure);
        assert_eq!(decoded[0].program.intent, "first");
        assert_eq!(decoded[0].doc, "Do first.");
        assert_eq!(decoded[0].input_docs, FIRST_DOCS);
        assert!(decoded[0].apis.is_empty());
        assert_eq!(decoded[1].program.name.as_str(), "test.program.two");
        assert_eq!(decoded[1].program.input, KindId(3));
        assert_eq!(decoded[1].program.result, KindId(4));
        assert_eq!(decoded[1].program.intent, "second");
        assert_eq!(decoded[1].doc, "Do second.");
        assert_eq!(decoded[1].input_docs, DocNode::Leaf);
        assert_eq!(decoded[1].apis, [ProgramApi::Http, ProgramApi::Workspace]);
    }

    #[test]
    fn repeated_program_name_is_refused() {
        // Catches a decoder that accepts duplicates, letting the driver's Programs::find silently pick the first.
        const FIRST: ProgramRecord<'static> = record(b"test.program.dup", 1, 2, 0, b"dup", b"Dup.", &LEAF);
        const SECOND: ProgramRecord<'static> = record(b"test.program.dup", 3, 4, 0, b"dup", b"Dup.", &LEAF);
        const LEN: usize = program_record_len(&FIRST);
        let first = write_program_record::<LEN>(&FIRST);
        let second = write_program_record::<LEN>(&SECOND);
        let mut section = first.to_vec();
        section.extend_from_slice(&second);

        assert_eq!(
            declarations(&section),
            Err(DeclarationsError::DuplicateName(ProgramName::new("test.program.dup").expect("valid test name")))
        );
    }

    #[test]
    fn a_record_with_an_unknown_api_bit_is_refused() {
        // Catches a decoder that silently drops an unknown bit, which would let a program bind an API the driver never
        // checks.
        const RECORD: ProgramRecord<'static> = record(b"test.program.api", 1, 2, 0b1000, b"api", b"Api.", &LEAF);
        const LEN: usize = program_record_len(&RECORD);
        let bytes = write_program_record::<LEN>(&RECORD);

        assert_eq!(declarations(&bytes), Err(DeclarationsError::UnknownApi(0b1000)));
    }
}
