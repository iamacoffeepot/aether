//! Mail kinds and adapters that turn a retained [`Owner`] into cluster input.
//!
//! Authors declare reactors; generated views and peer actors speak these kinds.
//! Journal payloads stay on the storage codec. Guards are never mailed.

use alloc::string::String;
use alloc::vec::Vec;

use aether_bloomery_kinds::{Entry, Seq};
use aether_bloomery_view::Heads;

use crate::error::PrepareError;
use crate::evaluate::{ArmVisitor, Output, Reactor};
use crate::owner::Owner;
use crate::params::Params;
use crate::trigger::Trigger;

/// Boot configuration for a generated views owner and its reactor peers.
///
/// `output` is the external mailbox that receives typed arm outputs. An empty
/// path drops outputs rather than guessing a destination.
#[aether_data::kind(name = "aether.bloomery.reactor.cluster_config", default, eq)]
pub struct ClusterConfig {
    /// Runtime-name address of the external output mailbox.
    pub output: String,
}

/// Portable journal envelope carried over mail. `bytes` are storage-codec
/// payload bytes, never positional [`aether_data::Kind::encode_into_bytes`].
#[derive(Clone, Debug, PartialEq, Eq, aether_data::Schema, serde::Serialize, serde::Deserialize)]
pub struct JournalEntry {
    /// Dense sequence assigned by the store.
    pub seq: u64,
    /// Stored kind name.
    pub kind: String,
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
            kind: entry.kind.clone(),
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
            kind: self.kind.clone(),
            cause: self.cause.map(Seq),
            recorded_at_millis: self.recorded_at_millis,
            bytes: self.bytes.clone(),
        }
    }
}

/// Contiguous entries pushed to a views owner. Each entry is prepared as its
/// own prefix so later live Event/EventBatch admission can keep per-event
/// boundaries.
#[aether_data::kind(name = "aether.bloomery.reactor.push_entries")]
pub struct PushEntries {
    /// Ordered journal envelopes.
    pub entries: Vec<JournalEntry>,
}

impl PushEntries {
    /// Wrap retained entries for mail.
    #[must_use]
    pub fn from_entries(entries: &[Entry]) -> Self {
        Self { entries: entries.iter().map(JournalEntry::from_entry).collect() }
    }
}

/// Aggregation outcome for one [`PushEntries`] request. Settlement of the
/// request is not proof of successful reactor evaluation.
#[aether_data::kind(name = "aether.bloomery.reactor.push_result")]
pub enum PushResult {
    /// Views advanced through `cursor`.
    Ok {
        /// Last retained sequence after the push.
        cursor: u64,
    },
    /// Prefix or fold failed before peers were prepared.
    Err {
        /// Display of the [`PrepareError`].
        message: String,
    },
}

impl PushResult {
    /// Convert a prepare/push outcome into the reply kind.
    #[must_use]
    pub fn from_prepare(result: Result<u64, PrepareError>) -> Self {
        match result {
            Ok(cursor) => Self::Ok { cursor },
            Err(error) => Self::Err { message: alloc::format!("{error}") },
        }
    }
}

/// Ask a views owner for its current aggregation cursor.
#[aether_data::kind(name = "aether.bloomery.reactor.cluster_status_query", default)]
pub struct ClusterStatusQuery;

/// Cursor of one cluster's bundled views. Not a durable execution checkpoint.
#[aether_data::kind(name = "aether.bloomery.reactor.cluster_status", eq)]
pub struct ClusterStatus {
    /// Last retained sequence, or `0` when empty.
    pub cursor: u64,
}

/// Owned prepared prefix mailed to a reactor peer.
///
/// Carries the trigger entry's storage bytes plus the bundled [`Heads`]
/// snapshot at that cursor. Named guards are resolved in the peer against
/// this snapshot; they are not serialized.
#[aether_data::kind(name = "aether.bloomery.reactor.prepared_prefix")]
pub struct PreparedPrefix {
    /// Trigger sequence.
    pub seq: u64,
    /// Stored trigger kind name.
    pub kind: String,
    /// Optional causing sequence.
    pub cause: Option<u64>,
    /// Wall clock at insert.
    pub recorded_at_millis: u64,
    /// Storage-codec trigger payload.
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
    /// Owned heads snapshot at `seq`.
    pub heads: Heads,
}

impl PreparedPrefix {
    /// Snapshot `entry` together with the views owner's heads at that cursor.
    #[must_use]
    pub fn from_entry(entry: &Entry, heads: Heads) -> Self {
        Self {
            seq: entry.seq.0,
            kind: entry.kind.clone(),
            cause: entry.cause.map(|seq| seq.0),
            recorded_at_millis: entry.recorded_at_millis,
            bytes: entry.bytes.clone(),
            heads,
        }
    }

    /// Rebuild the trigger envelope. Payload bytes stay storage-encoded.
    #[must_use]
    pub fn to_entry(&self) -> Entry {
        Entry {
            seq: Seq(self.seq),
            kind: self.kind.clone(),
            cause: self.cause.map(Seq),
            recorded_at_millis: self.recorded_at_millis,
            bytes: self.bytes.clone(),
        }
    }

    /// Owner that decodes this trigger and injects the mailed heads snapshot.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when the snapshot cursor does not match the entry.
    pub fn into_owner(self) -> Result<Owner, PrepareError> {
        let heads = self.heads;
        let mut owner = Owner::from_prepared(self.to_entry());
        if heads.cursor() == owner.cursor() {
            owner.install_published(heads)?;
        }
        Ok(owner)
    }
}

/// Fold every view named by `R`'s arms to the owner's current cursor.
///
/// # Errors
///
/// [`PrepareError`] from [`Owner::warm`].
pub fn warm_reactor<R: Reactor>(owner: &mut Owner) -> Result<(), PrepareError> {
    struct Warm<'a> {
        owner: &'a mut Owner,
        error: Result<(), PrepareError>,
    }

    impl ArmVisitor for Warm<'_> {
        fn visit<T, L, O>(&mut self, _name: &'static str)
        where
            T: Trigger,
            L: Params<T>,
            O: Output,
        {
            if self.error.is_ok() {
                self.error = self.owner.warm::<L::Views>();
            }
        }
    }

    let mut warm = Warm { owner, error: Ok(()) };
    R::visit_arms(&mut warm);
    warm.error
}
