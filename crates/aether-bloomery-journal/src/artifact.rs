//! Store-side artifact helpers that stay in the journal: DDL (artifacts and citation edges) and blob split.
//!
//! An artifact row records that a digest is stored, its size, and when; the
//! bytes live in the root's digest-named blob file, never in the row. A row,
//! not a file, is what makes an artifact stored. Kinds, framing, and digests
//! live in `aether-bloomery-kinds`. `split_artifact` stays here because it is
//! the store's read side and returns [`JournalError`].

use aether_data::KindId;
use aether_data::wire::WireDecode;

use crate::journal::JournalError;

pub const ARTIFACTS_DDL: &str = "
CREATE TABLE IF NOT EXISTS artifacts (
    digest BLOB PRIMARY KEY NOT NULL,
    size_bytes INTEGER NOT NULL,
    recorded_at_millis INTEGER NOT NULL
);
";

/// Citation edges recorded when an artifact is first stored; the closure walk reads them.
pub const CITATIONS_DDL: &str = "
CREATE TABLE IF NOT EXISTS citations (
    from_digest BLOB NOT NULL CHECK (length(from_digest) = 32),
    to_digest BLOB NOT NULL CHECK (length(to_digest) = 32),
    PRIMARY KEY (from_digest, to_digest)
) WITHOUT ROWID;
";

/// Split a stored blob into its kind prefix and payload.
///
/// # Errors
///
/// [`JournalError::CorruptArtifact`] when the blob is shorter than eight bytes
/// or the prefix does not decode as a [`KindId`].
pub fn split_artifact(bytes: &[u8]) -> Result<(KindId, &[u8]), JournalError> {
    if bytes.len() < 8 {
        return Err(JournalError::CorruptArtifact);
    }
    let (prefix, payload) = bytes.split_at(8);
    let mut cursor = prefix;
    let kind = KindId::decode(&mut cursor).map_err(|_| JournalError::CorruptArtifact)?;
    if cursor.is_empty() {
        Ok((kind, payload))
    } else {
        Err(JournalError::CorruptArtifact)
    }
}
