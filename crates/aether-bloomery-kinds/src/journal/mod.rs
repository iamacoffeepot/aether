//! Portable journal mail and the exact stored-entry envelope.
//!
//! Read mail lives here. [`EncodedArtifact`] carries one encoded value over
//! mail, and [`MoveHead`] / [`Publish`] / [`AppendRecords`] are the three
//! fenced write commands — the last is the one that carries a journal cause.
//! [`WatchHead`] is a long-poll watch on the head, answered once a
//! committed write moves it past the requested boundary. [`ReadClosure`]
//! reads an artifact's transitive closure over the journal's stored citation
//! edges under a validated [`ClosureLimit`], never truncating.

mod append;
mod artifact;
mod closure;
mod watch;
mod write;

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::KindId;

use crate::{ClosureArtifact, Digest, Entry, Seq};

pub use append::{AppendRecords, AppendRecordsResult, DriverRecord};
pub use artifact::{ArtifactCitation, EncodedArtifact};
pub use closure::{ClosureLimit, ClosureLimitError, ReadClosure, ReadClosureResult};
pub use watch::{WatchHead, WatchHeadResult};
pub use write::{MoveHead, MoveHeadResult, Publish, PublishResult};

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

/// One stored artifact as a [`ClosureArtifact`], or an explicit refusal.
#[aether_data::kind(name = "aether.bloomery.journal.read_artifact_result", no_serde)]
pub enum ReadArtifactResult {
    /// The stored artifact. Its claimed digest is the requested one, and a
    /// reader verifies it through [`ClosureArtifact::load`].
    Found {
        /// Stored kind, payload after the eight-byte kind prefix, and the
        /// digest the journal claims for them.
        artifact: ClosureArtifact,
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
