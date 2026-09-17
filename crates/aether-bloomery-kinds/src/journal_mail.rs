//! Portable journal query mail and the exact stored-entry envelope.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::KindId;

use crate::{Entry, ReactorLifecycleOutcome, Seq};

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

/// Record one reactor lifecycle observation through its journal owner.
#[aether_data::kind(name = "aether.bloomery.journal.record_reactor_lifecycle", eq, no_serde)]
pub struct RecordReactorLifecycle {
    /// Journal sequence that must still be the head.
    pub expect_head: u64,
    /// Recorded causing sequence, when the caller has one.
    pub cause: Option<u64>,
    /// Typed observation to append.
    pub event: ReactorLifecycleOutcome,
    /// Cited opaque artifact bytes, when not already stored.
    pub artifact_bytes: Option<Vec<u8>>,
}

/// Result of attempting one lifecycle observation append.
#[aether_data::kind(name = "aether.bloomery.journal.record_reactor_lifecycle_result", eq, no_serde)]
pub enum RecordReactorLifecycleResult {
    /// The observation committed at the assigned sequence.
    Committed {
        /// Sequence assigned by the journal.
        seq: u64,
    },
    /// The expected sequence was stale; no event or artifact was written.
    HeadMoved {
        /// Head observed by the append transaction.
        actual: u64,
    },
    /// Invalid staged bytes, citation, encoding, or backend failure.
    Err {
        /// Human-readable refusal.
        message: String,
    },
}
