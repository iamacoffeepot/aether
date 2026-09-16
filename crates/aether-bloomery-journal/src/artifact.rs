//! Artifact kinds and blob framing above a raw content-addressed store.
//!
//! These types move to `aether-bloomery-kinds` when that crate lands; the
//! journal re-exports them until then.
//!
//! The store stays raw: a row is `sha256(bytes)` plus the bytes. An artifact
//! is an abstraction above that row, whose bytes are an eight-byte [`KindId`]
//! prefix followed by a payload. The digest covers the kind, so a digest names
//! one kind and one payload.

use std::fmt;

use aether_data::wire::WireDecode;
use aether_data::{Kind, KindId};

use crate::journal::JournalError;
use sha2::{Digest as _, Sha256};

pub const ARTIFACTS_DDL: &str = "
CREATE TABLE IF NOT EXISTS artifacts (
    digest BLOB PRIMARY KEY NOT NULL,
    size_bytes INTEGER NOT NULL,
    recorded_at_millis INTEGER NOT NULL,
    bytes BLOB NOT NULL
);
";

/// 32-byte sha256 of a stored blob. A plain identity value: no leaf impls.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Borrow the 32 digest bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Wrap already-hashed digest bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Arbitrary bytes, no promise. Never instantiated; names a prefix.
pub struct OpaqueBytes;

impl Kind for OpaqueBytes {
    const NAME: &'static str = "bloomery.artifact.bytes";
    const ID: KindId = aether_data::storage_kind_id_from_name(Self::NAME);
}

/// UTF-8 text, validated when staged. Never instantiated; names a prefix.
pub struct Utf8Text;

impl Kind for Utf8Text {
    const NAME: &'static str = "bloomery.artifact.text";
    const ID: KindId = aether_data::storage_kind_id_from_name(Self::NAME);
}

/// Eight-byte little-endian [`KindId`] prefix. Same layout [`KindId`]'s wire encoding writes.
#[must_use]
pub fn artifact_prefix(kind: KindId) -> [u8; 8] {
    kind.0.to_le_bytes()
}

/// Prefix plus payload. The store hashes this whole blob.
#[must_use]
pub fn artifact_blob(kind: KindId, payload: &[u8]) -> Vec<u8> {
    let mut blob = artifact_prefix(kind).to_vec();
    blob.extend_from_slice(payload);
    blob
}

/// Digest of [`artifact_blob`].
#[must_use]
pub fn artifact_digest(kind: KindId, payload: &[u8]) -> Digest {
    hash_bytes(&artifact_blob(kind, payload))
}

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

/// Sha256 of a stored blob, prefix included.
pub fn hash_bytes(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Digest(hasher.finalize().into())
}
