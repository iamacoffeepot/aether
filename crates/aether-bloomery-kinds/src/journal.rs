//! Portable journal mail and the exact stored-entry envelope.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::{Citations, Cites, KindId, Storage, StorageData, StorageError};

use crate::{Digest, Entry, Head, RecordedHead, Seq};

/// One portable citation collected from an encoded artifact.
#[derive(Clone, Debug, PartialEq, Eq, aether_data::Schema)]
pub struct MoveHeadCitation {
    kind: KindId,
    bytes: Vec<u8>,
}

impl MoveHeadCitation {
    /// Expected kind of the cited artifact.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// Identity bytes of the cited artifact. The journal checks their width.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Take the citation's kind and identity bytes without copying them.
    #[must_use]
    pub fn into_parts(self) -> (KindId, Vec<u8>) {
        (self.kind, self.bytes)
    }
}

/// Publish one encoded value and move its typed head in a fenced journal append.
///
/// The typed constructor collects citations, but a decoded mail is untrusted:
/// the journal verifies every supplied citation and the destination, not that
/// the supplied list is complete for arbitrary encoded payloads.
#[aether_data::kind(name = "aether.bloomery.journal.move_head", eq, no_serde)]
pub struct MoveHead {
    head: RecordedHead,
    artifact_bytes: Vec<u8>,
    citations: Vec<MoveHeadCitation>,
    expected_seq: u64,
}

impl MoveHead {
    /// Encode `value` and collect its citations for one atomic publication.
    ///
    /// The head and value must have the same storage kind:
    ///
    /// ```compile_fail
    /// use aether_bloomery_kinds::{Head, MoveHead, Program, Tree};
    /// let head = Head::<Program>::new("main");
    /// let _ = MoveHead::new(&head, &Tree::empty(), 0);
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a storage error if `value` cannot be encoded.
    pub fn new<K: Storage + Clone + Cites>(head: &Head<K>, value: &K, expected_seq: u64) -> Result<Self, StorageError> {
        let mut citations = Citations::default();
        value.cites(&mut citations);
        Ok(Self {
            head: RecordedHead::from(head),
            artifact_bytes: K::encode_storage(&StorageData::from_value(value.clone()))?,
            citations: citations
                .into_vec()
                .into_iter()
                .map(|citation| MoveHeadCitation { kind: citation.kind, bytes: citation.bytes })
                .collect(),
            expected_seq,
        })
    }

    /// Head to move after the artifact is admitted.
    #[must_use]
    pub const fn head(&self) -> &RecordedHead {
        &self.head
    }

    /// Encoded storage payload without its kind prefix.
    #[must_use]
    pub fn artifact_bytes(&self) -> &[u8] {
        &self.artifact_bytes
    }

    /// Citations walked from the typed value.
    #[must_use]
    pub fn citations(&self) -> &[MoveHeadCitation] {
        &self.citations
    }

    /// Whole-journal sequence expected by the caller.
    #[must_use]
    pub const fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    /// Take the encoded publication and its fence without copying payload bytes.
    #[must_use]
    pub fn into_parts(self) -> (RecordedHead, Vec<u8>, Vec<MoveHeadCitation>, u64) {
        (self.head, self.artifact_bytes, self.citations, self.expected_seq)
    }
}

/// Outcome of a single fenced publication attempt.
#[aether_data::kind(name = "aether.bloomery.journal.move_head_result", eq, no_serde)]
pub enum MoveHeadResult {
    /// The event was appended at `seq`, and `artifact` names the admitted value.
    Committed { seq: u64, artifact: Digest },
    /// The supplied whole-journal fence was stale; nothing was written.
    Conflict { actual: u64 },
    /// Artifact admission or the journal backend refused the append.
    Err { message: String },
}

/// One recorded entry carried over mail. Payload bytes retain their storage encoding.
#[derive(Clone, Debug, PartialEq, Eq, aether_data::Schema, serde::Serialize, serde::Deserialize)]
pub struct JournalEntry {
    /// Dense sequence assigned by the store.
    pub seq: u64,
    /// Stored kind id.
    pub kind: KindId,
    /// Optional causing sequence.
    pub cause: Option<u64>,
    /// Wall clock at insert; folds ignore it.
    pub recorded_at_millis: u64,
    /// Verbatim storage-codec payload.
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
}

impl JournalEntry {
    /// Copy one retained [`Entry`] into the mail envelope.
    #[must_use]
    pub fn from_entry(entry: &Entry) -> Self {
        Self {
            seq: entry.seq.0,
            kind: entry.kind,
            cause: entry.cause.map(|seq| seq.0),
            recorded_at_millis: entry.recorded_at_millis,
            bytes: entry.bytes.clone(),
        }
    }

    /// Rebuild the portable [`Entry`]. Payload bytes stay storage-encoded.
    #[must_use]
    pub fn to_entry(&self) -> Entry {
        Entry {
            seq: Seq(self.seq),
            kind: self.kind,
            cause: self.cause.map(Seq),
            recorded_at_millis: self.recorded_at_millis,
            bytes: self.bytes.clone(),
        }
    }
}

/// Read entries after `after`, in stored sequence order.
#[aether_data::kind(name = "aether.bloomery.journal.read_events", copy, eq)]
pub struct ReadEvents {
    /// Exclusive sequence boundary; zero begins at the first entry.
    pub after: u64,
    /// Maximum number of entries requested.
    pub limit: u32,
}

/// Result of one journal page request.
#[aether_data::kind(name = "aether.bloomery.journal.read_events_result", eq)]
pub enum ReadEventsResult {
    /// Page and head observed by the same actor handler.
    Ok {
        /// The request's exclusive sequence boundary.
        after: u64,
        /// Journal head after the page was read.
        head: u64,
        /// Entries in ascending stored sequence order.
        entries: Vec<JournalEntry>,
    },
    /// Invalid limit or journal backend failure.
    Err {
        /// The request's exclusive sequence boundary.
        after: u64,
        /// Human-readable failure.
        message: String,
    },
}

/// Query the journal's current head.
#[aether_data::kind(name = "aether.bloomery.journal.read_head", default, eq)]
pub struct ReadHead;

/// Result of one head query.
#[aether_data::kind(name = "aether.bloomery.journal.read_head_result", eq)]
pub enum ReadHeadResult {
    /// Last stored sequence, or zero for an empty journal.
    Ok { head: u64 },
    /// Journal backend failure.
    Err { message: String },
}

/// Query one content-addressed artifact through its journal owner.
#[aether_data::kind(name = "aether.bloomery.journal.read_artifact", copy, eq, no_serde)]
pub struct ReadArtifact {
    /// Digest of the kind-prefixed stored blob.
    pub digest: Digest,
}

/// Kind and unprefixed payload of one stored artifact, or an explicit refusal.
#[aether_data::kind(name = "aether.bloomery.journal.read_artifact_result", eq, no_serde)]
pub enum ReadArtifactResult {
    /// Stored bytes whose kind and payload hash to the requested digest.
    Found {
        /// Requested digest.
        digest: Digest,
        /// Stored artifact kind.
        kind: KindId,
        /// Stored payload after the eight-byte kind prefix.
        bytes: Vec<u8>,
    },
    /// No artifact exists at the requested digest.
    Missing {
        /// Requested digest.
        digest: Digest,
    },
    /// Corrupt stored bytes or a journal backend failure.
    Err {
        /// Requested digest.
        digest: Digest,
        /// Human-readable failure.
        message: String,
    },
}
