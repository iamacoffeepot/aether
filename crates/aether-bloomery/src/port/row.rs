//! Codec for persisted outbox rows that have adopted the ADR-0059 storage
//! shape: one encode / decode pair, so a store or reactor site never picks
//! between encoders.
//!
//! The writing schema rides beside the bytes (`payload_schema` on the outbox
//! table). Absent, or [`POSITIONAL_ROW_SCHEMA`], is the pre-adoption
//! positional identity. The current identity is the row type's [`aether_data::Kind::NAME`].
//! Anything else is a named refusal — the same sentence
//! [`crate::decode_recorded_decisions`] produces for a journaled decision.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use aether_data::storage::{MAX_STORAGE_DEPTH, RecordReader, RecordWriter, fold_path_segment, terminate_field_hash};
use aether_data::wire::{WireDecode, WireEncode, decode_from_slice, from_bytes, to_vec};
use aether_data::{Schema, Storage, StorageData, StorageError, StorageLeaves};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::ids::WorkpieceId;
use crate::values::SpendQuiesce;
use crate::{BaseAlertView, Digest};

/// Pre-adoption positional identity. An absent stamp is this identity.
pub const POSITIONAL_ROW_SCHEMA: &str = "positional";

/// Why a persisted outbox row could not be folded into the current shape.
#[derive(Debug)]
pub enum RowSchemaError {
    /// The bytes did not decode as the shape the recorded identity named.
    Decode(String),
    /// The bytes could not be encoded as the shape the recorded identity named.
    Encode(String),
    /// The row names a writing schema this binary has no path for.
    NoUpcast {
        /// The kind the row is filed under.
        kind: &'static str,
        /// The identity stamped beside the bytes.
        found: String,
        /// The identity this binary writes.
        current: &'static str,
    },
}

impl fmt::Display for RowSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "persisted value did not decode: {error}"),
            Self::Encode(error) => write!(f, "persisted value did not encode: {error}"),
            Self::NoUpcast { kind, found, current } => {
                write!(f, "no migration from schema `{found}` to current `{current}` for kind `{kind}`")
            }
        }
    }
}

impl Error for RowSchemaError {}

/// Encode `value` under the writing-schema identity `schema`.
///
/// The current identity ([`aether_data::Kind::NAME`]) writes the storage shape. Absent or
/// [`POSITIONAL_ROW_SCHEMA`] writes the sealed positional shape. Anything else
/// is [`RowSchemaError::NoUpcast`].
///
/// # Errors
///
/// [`RowSchemaError::Encode`] when the bytes cannot be produced, and
/// [`RowSchemaError::NoUpcast`] when this binary has no encoder for `schema`.
pub fn encode_row<T: Storage + Serialize + Clone>(value: &T, schema: Option<&str>) -> Result<Vec<u8>, RowSchemaError> {
    match schema {
        None | Some(POSITIONAL_ROW_SCHEMA) => to_vec(value).map_err(|error| RowSchemaError::Encode(error.to_string())),
        Some(found) if found == T::NAME => T::encode_storage(&StorageData::from_value(value.clone()))
            .map_err(|error| RowSchemaError::Encode(error.to_string())),
        Some(found) => Err(RowSchemaError::NoUpcast { kind: T::NAME, found: found.to_owned(), current: T::NAME }),
    }
}

/// Decode persisted `bytes` under the writing-schema identity `schema`.
///
/// The current identity ([`aether_data::Kind::NAME`]) decodes the storage shape. Absent or
/// [`POSITIONAL_ROW_SCHEMA`] decodes the sealed positional shape. Anything else
/// is [`RowSchemaError::NoUpcast`].
///
/// # Errors
///
/// [`RowSchemaError::Decode`] when the bytes do not decode as the named shape,
/// and [`RowSchemaError::NoUpcast`] when this binary has no path for `schema`.
pub fn decode_row<T: Storage + DeserializeOwned>(bytes: &[u8], schema: Option<&str>) -> Result<T, RowSchemaError> {
    match schema {
        None | Some(POSITIONAL_ROW_SCHEMA) => {
            from_bytes(bytes).map_err(|error| RowSchemaError::Decode(error.to_string()))
        }
        Some(found) if found == T::NAME => {
            T::decode_storage(bytes).map(|data| data.value).map_err(|error| RowSchemaError::Decode(error.to_string()))
        }
        Some(found) => Err(RowSchemaError::NoUpcast { kind: T::NAME, found: found.to_owned(), current: T::NAME }),
    }
}

fn contribute_tuple0<T: StorageLeaves>(
    inner: &T,
    carry: u64,
    depth: u32,
    sink: &mut RecordWriter,
) -> Result<(), StorageError> {
    inner.contribute(fold_path_segment(carry, b"0", depth), depth + 1, sink)
}

fn assemble_tuple0<T: StorageLeaves>(carry: u64, depth: u32, source: &mut RecordReader) -> Result<T, StorageError> {
    T::assemble(fold_path_segment(carry, b"0", depth), depth + 1, source)
}

fn tuple0_absent<T: StorageLeaves>(carry: u64, depth: u32, source: &RecordReader) -> bool {
    T::is_absent(fold_path_segment(carry, b"0", depth), depth + 1, source)
}

impl StorageLeaves for Digest {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_tuple0(self.as_bytes(), carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_tuple0::<[u8; 32]>(carry, depth, source).map(Self::from_bytes)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        tuple0_absent::<[u8; 32]>(carry, depth, source)
    }
}

impl StorageLeaves for WorkpieceId {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_tuple0(&self.0, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_tuple0(carry, depth, source).map(Self)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        tuple0_absent::<String>(carry, depth, source)
    }
}

fn contribute_opaque<T: Schema + WireEncode>(
    value: &T,
    carry: u64,
    depth: u32,
    sink: &mut RecordWriter,
) -> Result<(), StorageError> {
    if depth > MAX_STORAGE_DEPTH {
        return Err(StorageError::NestingTooDeep);
    }
    let mut body = Vec::new();
    value.encode(&mut body).map_err(StorageError::from)?;
    sink.emit(terminate_field_hash(carry, &T::SCHEMA), body)
}

fn assemble_opaque<T>(carry: u64, depth: u32, source: &mut RecordReader) -> Result<T, StorageError>
where
    T: Schema + for<'de> WireDecode<'de>,
{
    if depth > MAX_STORAGE_DEPTH {
        return Err(StorageError::NestingTooDeep);
    }
    let hash = terminate_field_hash(carry, &T::SCHEMA);
    let body = source.take(hash).ok_or(StorageError::MissingRequiredField { hash, name: "" })?;
    decode_from_slice(&body).map_err(StorageError::from)
}

fn opaque_absent<T: Schema>(carry: u64, source: &RecordReader) -> bool {
    !source.contains(terminate_field_hash(carry, &T::SCHEMA))
}

impl StorageLeaves for SpendQuiesce {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_opaque(self, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_opaque(carry, depth, source)
    }

    fn is_absent(carry: u64, _depth: u32, source: &RecordReader) -> bool {
        opaque_absent::<Self>(carry, source)
    }
}

impl StorageLeaves for BaseAlertView {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.base.contribute(fold_path_segment(carry, b"base", depth), depth + 1, sink)?;
        self.tree.contribute(fold_path_segment(carry, b"tree", depth), depth + 1, sink)?;
        self.failed.contribute(fold_path_segment(carry, b"failed", depth), depth + 1, sink)?;
        self.evidence.contribute(fold_path_segment(carry, b"evidence", depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self {
            base: Digest::assemble(fold_path_segment(carry, b"base", depth), depth + 1, source)?,
            tree: Digest::assemble(fold_path_segment(carry, b"tree", depth), depth + 1, source)?,
            failed: Vec::assemble(fold_path_segment(carry, b"failed", depth), depth + 1, source)?,
            evidence: Digest::assemble(fold_path_segment(carry, b"evidence", depth), depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        Digest::is_absent(fold_path_segment(carry, b"base", depth), depth + 1, source)
            && Digest::is_absent(fold_path_segment(carry, b"tree", depth), depth + 1, source)
            && Vec::<String>::is_absent(fold_path_segment(carry, b"failed", depth), depth + 1, source)
            && Digest::is_absent(fold_path_segment(carry, b"evidence", depth), depth + 1, source)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{POSITIONAL_ROW_SCHEMA, decode_row, encode_row};
    use crate::{BaseAlertView, BloomView, Digest, SpendQuiesce, ViewDocument};
    use aether_data::Kind;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, PartialEq, Eq, Debug, aether_data::Storage, Serialize, Deserialize)]
    #[kind(name = "aether.bloomery.view_document")]
    struct ViewDocumentShadow {
        mainline: Digest,
        observed: Digest,
        spend_quiesce: Option<SpendQuiesce>,
        blooms: Vec<BloomView>,
        base_alert: Option<BaseAlertView>,
        extra: Option<u32>,
    }

    fn sample() -> ViewDocument {
        ViewDocument {
            mainline: Digest::from_bytes([1; 32]),
            observed: Digest::from_bytes([2; 32]),
            spend_quiesce: Some(SpendQuiesce::Window {
                window: String::from("day"),
                spent_micro_usd: 1,
                ceiling_micro_usd: 2,
            }),
            blooms: Vec::new(),
            base_alert: Some(BaseAlertView {
                base: Digest::from_bytes([3; 32]),
                tree: Digest::from_bytes([4; 32]),
                failed: vec![String::from("lint")],
                evidence: Digest::from_bytes([5; 32]),
            }),
        }
    }

    #[test]
    fn storage_row_tolerates_a_trailing_optional_in_either_direction() {
        // Tripwire: the row root adopted the storage shape. Encoding through
        // the positional path would make a longer reader fail on a shorter
        // payload and a shorter reader fail on a longer one.
        let produced = encode_row(&sample(), Some(ViewDocument::NAME)).unwrap();
        let shadow: ViewDocumentShadow = decode_row(&produced, Some(ViewDocumentShadow::NAME)).unwrap();
        assert!(shadow.extra.is_none(), "a reader with the extra field sees it absent");

        let newer = ViewDocumentShadow {
            mainline: shadow.mainline,
            observed: shadow.observed,
            spend_quiesce: shadow.spend_quiesce,
            blooms: shadow.blooms,
            base_alert: shadow.base_alert,
            extra: Some(7),
        };
        let newer_bytes = encode_row(&newer, Some(ViewDocumentShadow::NAME)).unwrap();
        let older: ViewDocument = decode_row(&newer_bytes, Some(ViewDocument::NAME)).unwrap();
        assert_eq!(older, sample(), "a reader without the extra field still decodes");
    }

    #[test]
    fn an_unknown_identity_refuses_by_name() {
        let error = decode_row::<ViewDocument>(b"x", Some("aether.bloomery.no-such-shape")).unwrap_err().to_string();
        assert!(error.contains("no migration from schema `aether.bloomery.no-such-shape`"), "{error}");
        assert!(error.contains(&format!("to current `{}`", ViewDocument::NAME)), "{error}");
        assert!(error.contains(&format!("for kind `{}`", ViewDocument::NAME)), "{error}");
    }

    #[test]
    fn positional_identity_still_decodes_the_serde_path() {
        let value = sample();
        let bytes = encode_row(&value, Some(POSITIONAL_ROW_SCHEMA)).unwrap();
        let decoded = decode_row::<ViewDocument>(&bytes, None).unwrap();
        assert_eq!(decoded, value);
    }
}
