//! TLV record envelope: `[field_hash u64][length u32][body]` (ADR-0059).
//!
//! Length is a fixed 32-bit little-endian count, matching every other
//! count in the ADR-0118 wire format. ADR-0059's draft drew a varint;
//! ADR-0118 removed variable-length integers, so the envelope follows
//! the format the rest of the crate already speaks.
//!
//! Writers emit records in field-hash ascending order and merge
//! bucketed unknowns into that same order, which is what makes a
//! decode-and-re-encode round trip byte-identical. Readers skip an
//! unrecognized record by its length without decoding the body.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use crate::wire;

/// One unrecognized TLV record, preserved verbatim so a re-encode can
/// merge it back in hash order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownField {
    /// Content hash that tagged the record on the wire.
    pub hash: u64,
    /// Verbatim TLV body, ready to re-emit.
    pub bytes: Vec<u8>,
}

/// Why a storage encode or decode failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    /// A required leaf was not present on the wire. Version skew: the
    /// sender's schema did not have this field.
    MissingRequiredField { hash: u64, name: &'static str },
    /// `Storage::STRICT` is set and the payload carried a field this
    /// schema does not bind.
    UnknownFieldInStrictMode { hash: u64 },
    /// The same field hash appeared twice in one payload.
    DuplicateField { hash: u64 },
    /// Input ended mid-record, or bytes remained after the last record
    /// that were too short to be a header.
    TrailingBytes,
    /// A leaf body failed the ADR-0188 owned codec.
    LeafBody(wire::Error),
    /// `__variant` named a discriminant this schema does not bind.
    UnknownVariant { hash: u64 },
    /// Nested flattening exceeded [`super::hash::MAX_STORAGE_DEPTH`].
    NestingTooDeep,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequiredField { hash, name: "" } => {
                write!(f, "storage: missing required field {hash:#018x}")
            }
            Self::MissingRequiredField { hash, name } => {
                write!(f, "storage: missing required field `{name}` ({hash:#018x})")
            }
            Self::UnknownFieldInStrictMode { hash } => {
                write!(f, "storage: unknown field {hash:#018x} in strict mode")
            }
            Self::DuplicateField { hash } => write!(f, "storage: duplicate field {hash:#018x}"),
            Self::TrailingBytes => f.write_str("storage: trailing bytes after TLV records"),
            Self::LeafBody(err) => write!(f, "storage: leaf body: {err}"),
            Self::UnknownVariant { hash } => write!(f, "storage: unknown enum variant {hash:#018x}"),
            Self::NestingTooDeep => f.write_str("storage: nested flattening exceeded depth cap"),
        }
    }
}

impl StdError for StorageError {}

impl From<wire::Error> for StorageError {
    fn from(err: wire::Error) -> Self {
        Self::LeafBody(err)
    }
}

const HEADER_LEN: usize = 8 + 4;

/// Accumulates TLV records and emits them in hash-ascending order.
#[derive(Default)]
pub struct RecordWriter {
    records: BTreeMap<u64, Vec<u8>>,
}

impl RecordWriter {
    /// Empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self { records: BTreeMap::new() }
    }

    /// Record one leaf. Fails if `hash` was already emitted.
    pub fn emit(&mut self, hash: u64, body: Vec<u8>) -> Result<(), StorageError> {
        if self.records.insert(hash, body).is_some() {
            return Err(StorageError::DuplicateField { hash });
        }
        Ok(())
    }

    /// Merge previously bucketed unknowns into the same sort order.
    pub fn merge_unknown(&mut self, unknowns: &[UnknownField]) -> Result<(), StorageError> {
        for unknown in unknowns {
            self.emit(unknown.hash, unknown.bytes.clone())?;
        }
        Ok(())
    }

    /// Serialize every record as `[hash u64][len u32][body]`.
    pub fn finish(self) -> Result<Vec<u8>, StorageError> {
        let mut out = Vec::new();
        for (hash, body) in self.records {
            let len = u32::try_from(body.len()).map_err(|_| StorageError::from(wire::Error::Length))?;
            out.extend_from_slice(&hash.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&body);
        }
        Ok(out)
    }
}

/// Parsed TLV records, keyed by field hash.
pub struct RecordReader {
    records: BTreeMap<u64, Vec<u8>>,
}

impl RecordReader {
    /// Walk `bytes` into a hash-keyed map. Duplicate hashes and a
    /// truncated header or body are errors; an unrecognized record is
    /// kept so the caller can bucket or reject it.
    pub fn parse(mut bytes: &[u8]) -> Result<Self, StorageError> {
        let mut records = BTreeMap::new();
        while !bytes.is_empty() {
            if bytes.len() < HEADER_LEN {
                return Err(StorageError::TrailingBytes);
            }
            let mut hash_bytes = [0u8; 8];
            hash_bytes.copy_from_slice(&bytes[..8]);
            let hash = u64::from_le_bytes(hash_bytes);
            let mut len_bytes = [0u8; 4];
            len_bytes.copy_from_slice(&bytes[8..HEADER_LEN]);
            let len = u32::from_le_bytes(len_bytes) as usize;
            bytes = &bytes[HEADER_LEN..];
            if bytes.len() < len {
                return Err(StorageError::TrailingBytes);
            }
            let (body, rest) = bytes.split_at(len);
            bytes = rest;
            if records.insert(hash, body.to_vec()).is_some() {
                return Err(StorageError::DuplicateField { hash });
            }
        }
        Ok(Self { records })
    }

    /// Whether a record with this hash is still unread.
    #[must_use]
    pub fn contains(&self, hash: u64) -> bool {
        self.records.contains_key(&hash)
    }

    /// Take the body for `hash`, if present.
    pub fn take(&mut self, hash: u64) -> Option<Vec<u8>> {
        self.records.remove(&hash)
    }

    /// Remaining records, in hash order, as the unknown-field bucket.
    #[must_use]
    pub fn into_unknown(self) -> Vec<UnknownField> {
        self.records.into_iter().map(|(hash, bytes)| UnknownField { hash, bytes }).collect()
    }

    /// Error if any record remains (strict mode).
    pub fn reject_remaining(self) -> Result<(), StorageError> {
        match self.records.into_iter().next() {
            Some((hash, _)) => Err(StorageError::UnknownFieldInStrictMode { hash }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn writer_emits_records_in_hash_order() {
        // A writer that emitted declaration order would make an older
        // reader's re-encode of a newer payload differ from the newer
        // writer's original bytes.
        let mut writer = RecordWriter::new();
        writer.emit(0x20, b"second".to_vec()).unwrap();
        writer.emit(0x10, b"first".to_vec()).unwrap();
        writer.emit(0x30, b"third".to_vec()).unwrap();
        let bytes = writer.finish().unwrap();

        let mut expected = RecordWriter::new();
        expected.emit(0x10, b"first".to_vec()).unwrap();
        expected.emit(0x20, b"second".to_vec()).unwrap();
        expected.emit(0x30, b"third".to_vec()).unwrap();
        assert_eq!(bytes, expected.finish().unwrap());

        let mut hashes = Vec::new();
        let mut cursor = bytes.as_slice();
        while !cursor.is_empty() {
            hashes.push(u64::from_le_bytes(cursor[..8].try_into().unwrap()));
            let len = u32::from_le_bytes(cursor[8..12].try_into().unwrap()) as usize;
            cursor = &cursor[12 + len..];
        }
        assert_eq!(hashes, [0x10, 0x20, 0x30]);
    }

    #[test]
    fn merge_unknown_interleaves_by_hash() {
        let mut writer = RecordWriter::new();
        writer.emit(0x10, b"a".to_vec()).unwrap();
        writer.emit(0x30, b"c".to_vec()).unwrap();
        writer.merge_unknown(&[UnknownField { hash: 0x20, bytes: b"b".to_vec() }]).unwrap();
        let parsed = RecordReader::parse(&writer.finish().unwrap()).unwrap();
        assert!(parsed.contains(0x10));
        assert!(parsed.contains(0x20));
        assert!(parsed.contains(0x30));
    }

    #[test]
    fn reader_skips_unknown_by_length() {
        // If the reader consumed an unknown record's body as the next
        // header, every subsequent field would misread. The unknown
        // body's bytes look like a plausible header on purpose.
        let mut writer = RecordWriter::new();
        writer.emit(0x01, 42u64.to_le_bytes().to_vec()).unwrap();
        let mut decoy = Vec::new();
        decoy.extend_from_slice(&0xFFFFu64.to_le_bytes());
        decoy.extend_from_slice(&4u32.to_le_bytes());
        decoy.extend_from_slice(&0u32.to_le_bytes());
        writer.emit(0x99, decoy).unwrap();
        writer.emit(0x02, 7u64.to_le_bytes().to_vec()).unwrap();
        let bytes = writer.finish().unwrap();

        let mut reader = RecordReader::parse(&bytes).unwrap();
        assert_eq!(reader.take(0x01).unwrap(), 42u64.to_le_bytes());
        assert_eq!(reader.take(0x02).unwrap(), 7u64.to_le_bytes());
        let unknown = reader.into_unknown();
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].hash, 0x99);
    }

    #[test]
    fn duplicate_hash_is_an_error() {
        let mut writer = RecordWriter::new();
        writer.emit(0x01, b"a".to_vec()).unwrap();
        assert!(matches!(writer.emit(0x01, b"b".to_vec()), Err(StorageError::DuplicateField { hash: 0x01 })));
    }

    #[test]
    fn truncated_header_is_trailing_bytes() {
        assert!(matches!(RecordReader::parse(&[0, 1, 2]), Err(StorageError::TrailingBytes)));
    }
}
