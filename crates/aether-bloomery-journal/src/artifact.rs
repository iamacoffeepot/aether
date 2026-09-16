//! Content-addressed artifacts in the same SQLite database as the event log.

use std::collections::HashMap;
use std::slice;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::{LabelNode, Schema, SchemaType, StorageLeaves};
use rusqlite::{TransactionBehavior, params, params_from_iter};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::journal::{Journal, JournalError, sqlite_i64};

pub(crate) const ARTIFACTS_DDL: &str = "
CREATE TABLE IF NOT EXISTS artifacts (
    digest BLOB PRIMARY KEY NOT NULL,
    size_bytes INTEGER NOT NULL,
    recorded_at_millis INTEGER NOT NULL,
    bytes BLOB NOT NULL
);
";

/// 32-byte sha256 of an artifact's bytes.
///
/// Implemented as a transparent `[u8; 32]` array leaf so a `Storage` event can
/// carry it as a field. `#[derive(Schema)]` on a tuple struct would emit a
/// one-field struct, not the array leaf the issue asked for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Digest(pub [u8; 32]);

impl Schema for Digest {
    const SCHEMA: SchemaType = <[u8; 32] as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Digest"));
    const LABEL_NODE: LabelNode = <[u8; 32] as Schema>::LABEL_NODE;
}

impl StorageLeaves for Digest {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        <[u8; 32] as StorageLeaves>::contribute(&self.0, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self(<[u8; 32] as StorageLeaves>::assemble(carry, depth, source)?))
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        <[u8; 32] as StorageLeaves>::is_absent(carry, depth, source)
    }
}

impl StorageElement for Digest {
    const TAGGED: bool = <[u8; 32] as StorageElement>::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.0.contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Ok(Self(<[u8; 32] as StorageElement>::assemble_element(depth, cursor)?))
    }
}

impl Journal {
    /// Store `bytes` under `sha256(bytes)`. Idempotent: the same bytes return the
    /// same digest and write nothing the second time.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend failure.
    pub fn put_artifact(&mut self, bytes: &[u8]) -> Result<Digest, JournalError> {
        self.put_artifacts(&[bytes])?.into_iter().next().ok_or(JournalError::IntegerRange)
    }

    /// Load one artifact. `Ok(None)` when the digest was never put.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend failure.
    pub fn get_artifact(&self, digest: &Digest) -> Result<Option<Vec<u8>>, JournalError> {
        self.get_artifacts(slice::from_ref(digest))?.into_iter().next().ok_or(JournalError::IntegerRange)
    }

    /// Store each item in one transaction. Digests are returned in input order.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend failure. Nothing commits.
    pub fn put_artifacts(&mut self, items: &[&[u8]]) -> Result<Vec<Digest>, JournalError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recorded_at_millis = sqlite_i64(self.clock.now_millis())?;
        let mut out = Vec::with_capacity(items.len());
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO artifacts (digest, size_bytes, recorded_at_millis, bytes) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for item in items {
                let digest = hash_bytes(item);
                let size_bytes = sqlite_i64(u64::try_from(item.len()).map_err(|_| JournalError::IntegerRange)?)?;
                stmt.execute(params![digest.0.as_slice(), size_bytes, recorded_at_millis, *item])?;
                out.push(digest);
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// Load many artifacts in one query. Results are in input order; absent
    /// digests are `None`.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend failure, never a short result.
    pub fn get_artifacts(&self, digests: &[Digest]) -> Result<Vec<Option<Vec<u8>>>, JournalError> {
        if digests.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; digests.len()].join(", ");
        let sql = format!("SELECT digest, bytes FROM artifacts WHERE digest IN ({placeholders})");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(digests.iter().map(|digest| digest.0.as_slice())))?;
        let mut found = HashMap::with_capacity(digests.len());
        while let Some(row) = rows.next()? {
            let raw: Vec<u8> = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            let key = Digest(raw.try_into().map_err(|_| JournalError::CorruptArtifactDigest)?);
            found.insert(key, bytes);
        }
        Ok(digests.iter().map(|digest| found.get(digest).cloned()).collect())
    }
}

fn hash_bytes(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Digest(hasher.finalize().into())
}
