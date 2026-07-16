//! The `aether.store.*` transact-mail kind family (ADR-0149 §The boundary).
//!
//! These are the typed requests the guest (or an external RPC client) sends to
//! the `aether.store` mailbox; the guest never sees SQL. Every request has a
//! matching `Ok`/`Err` reply enum, following the clipboard/fs capability
//! precedent.
//!
//! The bloom-protocol value types ([`aether_bloomery::Event`] and friends) are
//! carried as their canonical [`aether_data::wire`] bytes rather than typed
//! fields, because those value types are serde-encoded but not `Schema` (the
//! store journals opaque bytes keyed by idempotency key; the host decodes them
//! through the reducer on replay). The dedup axis (`idempotency_key`) and the
//! seal axis (`members` workpieces) are typed, since the store keys on them.

use serde::{Deserialize, Serialize};

/// Append one journal event, deduplicated by its idempotency key (the inbox).
/// A key already recorded is a no-op that reports [`AppendEventResult::Duplicate`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.append_event")]
pub struct AppendEvent {
    /// The event's idempotency key — the inbox dedup axis.
    pub idempotency_key: String,
    /// The event's canonical `aether_data::wire` bytes (an encoded
    /// [`aether_bloomery::Event`]).
    pub event: Vec<u8>,
}

/// Reply to [`AppendEvent`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.append_event_result")]
pub enum AppendEventResult {
    /// The event was appended at this monotonic journal sequence.
    Applied {
        /// The journal sequence the event landed at.
        sequence: u64,
    },
    /// The idempotency key was already recorded — no event was appended.
    Duplicate,
    /// The append failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Claim active membership for a bloom's workpieces under the
/// at-most-one-active-bloom-per-workpiece uniqueness constraint. All-or-nothing:
/// the whole set inserts in one transaction or none of it does.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.claim_seal")]
pub struct ClaimSeal {
    /// The sealing bloom's id (its digest's raw bytes).
    pub bloom: Vec<u8>,
    /// The workpieces the bloom claims.
    pub members: Vec<String>,
}

/// Reply to [`ClaimSeal`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.claim_seal_result")]
pub enum ClaimSealResult {
    /// Every workpiece was claimed for the bloom — the seal is durable.
    Sealed,
    /// A workpiece was already claimed by an active bloom; the whole seal
    /// aborted and claimed nothing.
    Conflict {
        /// The first conflicting workpiece.
        workpiece: String,
    },
    /// The claim failed for a non-conflict reason.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Atomically supersede a predecessor with a successor: release the
/// predecessor's active memberships and claim the successor's members, in one
/// transaction. A workpiece the predecessor freed can be re-claimed by the
/// successor; a workpiece held by a *third* active bloom conflicts and the whole
/// supersession rolls back.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.supersede")]
pub struct Supersede {
    /// The predecessor bloom whose memberships are released.
    pub predecessor: Vec<u8>,
    /// The successor bloom that claims the new membership set.
    pub successor: Vec<u8>,
    /// The successor's workpieces.
    pub members: Vec<String>,
}

/// Reply to [`Supersede`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.supersede_result")]
pub enum SupersedeResult {
    /// The predecessor was released and the successor claimed — atomically.
    Sealed,
    /// A successor workpiece is held by a third active bloom; the whole
    /// supersession rolled back (the predecessor keeps its claims).
    Conflict {
        /// The first conflicting workpiece.
        workpiece: String,
    },
    /// The supersession failed for a non-conflict reason.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Release every active membership a bloom holds (on land or supersession).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.release_membership")]
pub struct ReleaseMembership {
    /// The bloom whose memberships are released.
    pub bloom: Vec<u8>,
}

/// Reply to [`ReleaseMembership`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.release_membership_result")]
pub enum ReleaseMembershipResult {
    /// The memberships were released.
    Ok {
        /// How many workpieces the bloom held.
        released: u32,
    },
    /// The release failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Enqueue one entry onto the transactional outbox for later republish.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.enqueue_outbox")]
pub struct EnqueueOutbox {
    /// A caller-defined topic naming what the payload is (e.g. the outbound
    /// kind name), so a republisher can route it.
    pub topic: String,
    /// The opaque payload bytes to republish.
    pub payload: Vec<u8>,
}

/// Reply to [`EnqueueOutbox`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.enqueue_outbox_result")]
pub enum EnqueueOutboxResult {
    /// The entry was enqueued at this outbox sequence.
    Ok {
        /// The outbox sequence the entry landed at.
        sequence: u64,
    },
    /// The enqueue failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Read every undelivered outbox entry, in sequence order (non-destructive —
/// republish, then [`AckOutbox`] the delivered range).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.drain_outbox")]
pub struct DrainOutbox;

/// One undelivered outbox entry.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    /// The entry's outbox sequence.
    pub sequence: u64,
    /// The caller-defined topic.
    pub topic: String,
    /// The opaque payload bytes.
    pub payload: Vec<u8>,
}

/// Reply to [`DrainOutbox`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.drain_outbox_result")]
pub enum DrainOutboxResult {
    /// The undelivered entries, in sequence order.
    Ok {
        /// The undelivered outbox entries.
        entries: Vec<OutboxEntry>,
    },
    /// The drain failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Mark every outbox entry up to and including `through_sequence` as delivered.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.ack_outbox")]
pub struct AckOutbox {
    /// Acknowledge every outbox entry with a sequence at or below this.
    pub through_sequence: u64,
}

/// Reply to [`AckOutbox`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.ack_outbox_result")]
pub enum AckOutboxResult {
    /// The entries were acknowledged.
    Ok {
        /// How many entries were newly marked delivered.
        acked: u32,
    },
    /// The ack failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

// The journal-replay transact-mails — `ReplayJournal`, `JournalRecord`,
// `ReplayJournalResult` — live in `aether_bloomery::control` alongside `Commit`,
// not here: the wasm control actor (`aether-bloomery`) sends `ReplayJournal` at
// boot and folds the reply, and `aether-bloomery-host` depends on
// `aether-bloomery`, so the reverse edge would be a package cycle. One
// definition both sides share; the `#[kind(name = "aether.store.replay_journal…")]`
// literal is the wire identity wherever the type is declared (issue #3497).
