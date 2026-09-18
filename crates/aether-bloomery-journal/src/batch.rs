//! The only write: staged artifacts plus events, judged together by `append`.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use aether_bloomery_kinds::{Digest, EncodedArtifact, OpaqueBytes, Ref, Utf8Text, artifact_blob, hash_bytes};
use aether_data::{Citation, Citations, Cites, Kind, Storage, StorageData, StorageError};

use crate::Seq;
use crate::draft::{Draft, DraftError};

/// One staged blob: digest, prefixed bytes, and citations walked from an encoded value.
pub struct Staged {
    pub digest: Digest,
    pub bytes: Vec<u8>,
    pub citations: Vec<Citation>,
}

/// Staged blobs plus encoded events. `append` is the only judge.
pub struct Batch {
    pub(crate) staged: Vec<Staged>,
    seen: HashSet<Digest>,
    pub(crate) events: Vec<Draft>,
}

impl Batch {
    /// Empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self { staged: Vec::new(), seen: HashSet::new(), events: Vec::new() }
    }

    /// Stage `payload` as [`OpaqueBytes`]. Identical blob bytes in one batch are one entry.
    pub fn stage_bytes(&mut self, payload: &[u8]) -> Ref<OpaqueBytes> {
        Ref::from_digest(self.insert_blob(artifact_blob(OpaqueBytes::ID, payload), Vec::new()))
    }

    /// Stage UTF-8 `text` as [`Utf8Text`]. Identical blob bytes in one batch are one entry.
    pub fn stage_text(&mut self, text: &str) -> Ref<Utf8Text> {
        Ref::from_digest(self.insert_blob(artifact_blob(Utf8Text::ID, text.as_bytes()), Vec::new()))
    }

    /// Encode `value`, prefix `K::ID`, and walk it for citations.
    ///
    /// # Errors
    ///
    /// [`BatchError::Storage`] when encoding fails.
    pub fn stage_encoded<K: Storage + Clone + Cites>(&mut self, value: &K) -> Result<Ref<K>, BatchError> {
        let mut sink = Citations::default();
        value.cites(&mut sink);
        let payload = K::encode_storage(&StorageData::from_value(value.clone())).map_err(BatchError::Storage)?;
        Ok(Ref::from_digest(self.insert_blob(artifact_blob(K::ID, &payload), sink.into_vec())))
    }

    /// Stage an already-encoded artifact under its own kind prefix.
    ///
    /// Identical blob bytes in one batch are one entry. Its citations are
    /// verified by `append` like any other staged blob's.
    pub fn stage_artifact(&mut self, artifact: EncodedArtifact) -> Digest {
        let (kind, payload, citations) = artifact.into_parts();
        self.insert_blob(
            artifact_blob(kind, &payload),
            citations
                .into_iter()
                .map(|citation| {
                    let (kind, bytes) = citation.into_parts();
                    Citation { kind, bytes }
                })
                .collect(),
        )
    }

    /// Encode `event` and push it.
    ///
    /// # Errors
    ///
    /// [`BatchError::Storage`] when encoding fails.
    pub fn push_event<K: Storage + Clone + Cites>(&mut self, event: &K, cause: Option<Seq>) -> Result<(), BatchError> {
        self.push_draft(Draft::of(event, cause)?);
        Ok(())
    }

    /// Push an already-encoded draft.
    pub fn push_draft(&mut self, draft: Draft) {
        self.events.push(draft);
    }

    /// Citations recorded on the staged blob named by `digest`.
    #[must_use]
    pub fn staged_citations(&self, digest: &Digest) -> Option<&[Citation]> {
        self.lookup(digest).map(|staged| staged.citations.as_slice())
    }

    /// Prefixed bytes of the staged blob named by `digest`.
    #[must_use]
    pub fn staged_blob(&self, digest: &Digest) -> Option<&[u8]> {
        self.lookup(digest).map(|staged| staged.bytes.as_slice())
    }

    /// True when the batch stages nothing and carries no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.events.is_empty()
    }

    fn lookup(&self, digest: &Digest) -> Option<&Staged> {
        self.staged.iter().find(|staged| staged.digest == *digest)
    }

    fn insert_blob(&mut self, bytes: Vec<u8>, citations: Vec<Citation>) -> Digest {
        let digest = hash_bytes(&bytes);
        if self.seen.insert(digest) {
            self.staged.push(Staged { digest, bytes, citations });
        }
        digest
    }
}

impl Default for Batch {
    fn default() -> Self {
        Self::new()
    }
}

/// Failure to encode a staged value or event.
#[derive(Debug)]
pub enum BatchError {
    /// [`Storage::encode_storage`] refused the value.
    Storage(StorageError),
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "failed to encode batch value: {error}"),
        }
    }
}

impl Error for BatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
        }
    }
}

impl From<DraftError> for BatchError {
    fn from(error: DraftError) -> Self {
        match error {
            DraftError::Storage(error) => Self::Storage(error),
        }
    }
}
