//! The only write: staged artifacts plus events, judged together by `append`.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use aether_bloomery_kinds::{
    ArtifactCitation, Digest, EncodedArtifact, OpaqueBytes, Ref, Transition, Utf8Text, artifact_blob, artifact_prefix,
    hash_bytes,
};
use aether_data::{
    Blob, BlobReader, Citation, Citations, Cites, Kind, KindId, MAX_READ_BYTES, Storage, StorageData, StorageError,
};

use crate::Seq;
use crate::runtime::draft::{Draft, DraftError};

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
    pub(crate) required: Vec<Digest>,
    not_before_millis: u64,
}

impl Batch {
    /// Empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            staged: Vec::new(),
            seen: HashSet::new(),
            events: Vec::new(),
            required: Vec::new(),
            not_before_millis: 0,
        }
    }

    /// Floor the journal time `append` stamps this batch's entries with at
    /// `millis` (ADR-0245). A batch with no floor is stamped at the greater of
    /// the latest recorded time and the clock.
    pub fn not_before(&mut self, millis: u64) {
        self.not_before_millis = millis;
    }

    /// The journal time floor of this batch's entries; `0` for none.
    #[must_use]
    pub const fn not_before_millis(&self) -> u64 {
        self.not_before_millis
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
    /// The payload is read out of its [`Blob`] a [`BlobReader`] window at a
    /// time into the batch's one prefixed buffer; the fenced writes carry
    /// small driver and program values. Identical blob bytes in one batch are
    /// one entry. Its citations are verified by `append` like any other
    /// staged blob's.
    pub fn stage_artifact(&mut self, artifact: EncodedArtifact) -> Digest {
        let (kind, payload, artifact_citations) = artifact.into_parts();
        self.insert_blob(prefixed(kind, &payload), citations(artifact_citations))
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

    /// Push a driver's `Transition` under `cause`, requiring its input and
    /// result and recording both as the entry's citations, input first.
    ///
    /// # Errors
    ///
    /// [`BatchError::Storage`] when encoding fails.
    pub(crate) fn push_transition(&mut self, record: &Transition, cause: Seq) -> Result<(), BatchError> {
        self.require_artifact(record.input);
        self.require_artifact(record.result);
        self.push_draft(Draft::of(record, Some(cause))?.citing_untyped([record.input, record.result]));
        Ok(())
    }

    /// Require that `digest` be stored or staged when `append` commits.
    ///
    /// Existence only, no prefix. `Transition.input` / `.result` use this:
    /// their stored kind is known only from the cited program at runtime,
    /// so no citation walk can cover them.
    pub(crate) fn require_artifact(&mut self, digest: Digest) {
        self.required.push(digest);
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

    /// True when the batch stages nothing, carries no events, and requires no artifact.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.events.is_empty() && self.required.is_empty()
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

/// The store's citations for an artifact's carried ones, moving their bytes.
pub fn citations(carried: Vec<ArtifactCitation>) -> Vec<Citation> {
    carried
        .into_iter()
        .map(|citation| {
            let (kind, bytes) = citation.into_parts();
            Citation { kind, bytes }
        })
        .collect()
}

/// `kind`'s prefix followed by every byte of `payload`, each window read
/// straight into its place and no window larger than what remains.
fn prefixed(kind: KindId, payload: &Blob) -> Vec<u8> {
    let reader = BlobReader::open(payload);
    let mut blob = artifact_prefix(kind).to_vec();
    let mut offset = 0;
    loop {
        let start = blob.len();
        let rest = reader.len().saturating_sub(offset);
        blob.resize(start + usize::try_from(rest).map_or(MAX_READ_BYTES, |rest| rest.min(MAX_READ_BYTES)), 0);
        let copied = reader.read_range(offset, &mut blob[start..]);
        blob.truncate(start + copied);
        if copied == 0 {
            return blob;
        }
        offset += copied as u64;
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
