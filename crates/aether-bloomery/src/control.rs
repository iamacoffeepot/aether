//! The control loop — admitted events driven through the reducer with
//! boot-time journal replay (ADR-0149 §The control core, §Migration step 1).
//!
//! This module carries two things: the control-plane mail vocabulary (always
//! compiled, so both the native host and the wasm control actor share one
//! definition), and — behind the `runtime` feature — the [`ControlCore`] wasm
//! actor that owns the live [`Snapshot`](crate::reduce::Snapshot), drives
//! [`reduce`](crate::reduce::reduce), and commits decisions through the
//! `aether.store` capability.
//!
//! # Why the store transact-mails the actor drives live here
//!
//! [`Commit`] and the [`ReplayJournal`] family are the store's own
//! transact-mails (ADR-0149 §The boundary), but they are *defined* here in
//! `aether-bloomery` rather than in `aether-bloomery-host` alongside the rest
//! of the `aether.store.*` family. The wasm [`ControlCore`] actor lives in this
//! crate and must construct and send them; `aether-bloomery-host` depends on
//! `aether-bloomery` (for the reducer and value vocabulary), so the reverse
//! edge would be a package cycle. Defining these kinds here keeps one
//! definition both sides share — the host's `StoreCapability` imports them
//! inward, cycle-free, exactly as it imports [`Event`](crate::reduce::Event).
//! The wire contract is identical wherever the type is declared: the
//! `#[kind(name = "…")]` literal is the identity.
//!
//! Like the rest of the `aether.store.*` family, these kinds carry the
//! bloom-protocol payloads as their canonical [`aether_data::wire`] bytes (the
//! journaled [`Event`](crate::reduce::Event), the outbox receipt payloads)
//! rather than typed fields — the store journals opaque bytes and the reducer
//! decodes them on replay. The membership mutations the store applies to its
//! `active_membership` table are the typed axes it keys on, mirroring
//! `ClaimSeal` / `Supersede`.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// One active-membership mutation the store applies inside the combined
/// [`Commit`] transaction: a workpiece claimed (or released) for a bloom. The
/// bloom is its [`BloomId`](crate::ids::BloomId) digest's raw bytes, matching
/// the opaque-bytes convention `ClaimSeal` / `Supersede` already use for the
/// `bloom` axis.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MembershipMutation {
    /// The workpiece whose active membership changes.
    pub workpiece: String,
    /// The bloom the claim attaches to (or releases from) — its digest bytes.
    pub bloom: Vec<u8>,
}

/// One outbox entry the combined [`Commit`] enqueues inside its transaction —
/// a caller-defined topic plus opaque payload bytes, carried inline so the
/// enqueue is atomic with the journal append.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutboxPayload {
    /// A caller-defined topic naming what the payload is, so a republisher can
    /// route it.
    pub topic: String,
    /// The opaque payload bytes to republish.
    pub payload: Vec<u8>,
}

/// The combined atomic store commit (ADR-0149 §The control core). One
/// transact-mail carrying the idempotency-keyed journal event plus the
/// reducer's membership mutations and outbox payloads, applied in a **single**
/// `SQLite` transaction. This is the primitive the wasm control actor drives
/// after reducing an event — "one store transaction" cannot be assembled from
/// the separate `append_event` / `claim_seal` / `enqueue_outbox` mails (three
/// transactions; a crash between them breaks atomicity), and a wasm actor
/// cannot hold a `SQLite` transaction open across host round-trips.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.commit")]
pub struct Commit {
    /// The event's idempotency key — the inbox dedup axis. A key already
    /// journaled makes the whole commit a [`CommitResult::Duplicate`] no-op.
    pub idempotency_key: String,
    /// The event's canonical `aether_data::wire` bytes (an encoded
    /// [`Event`](crate::reduce::Event)) — the durable replay source.
    pub event: Vec<u8>,
    /// The workpieces this decision releases from their blooms. Applied before
    /// the claims, so a superseding successor can reclaim a workpiece its
    /// predecessor freed in the same transaction.
    pub releases: Vec<MembershipMutation>,
    /// The workpieces this decision claims for their blooms, under the
    /// at-most-one-active-bloom-per-workpiece uniqueness constraint. A conflict
    /// on any one aborts the whole commit.
    pub claims: Vec<MembershipMutation>,
    /// The outbox entries this decision enqueues (e.g. a landing receipt).
    pub outbox: Vec<OutboxPayload>,
}

/// Reply to [`Commit`]. Echoes the `idempotency_key` so the control actor can
/// correlate the reply to the admit it is still holding a reply handle for —
/// the store is addressed by runtime name (`send_to_named`), which carries no
/// typed reply context, so the key is the correlation axis.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.commit_result")]
pub enum CommitResult {
    /// The whole decision committed atomically at this journal sequence.
    Applied {
        /// The idempotency key of the committed event (correlation).
        idempotency_key: String,
        /// The journal sequence the event landed at.
        sequence: u64,
    },
    /// The idempotency key was already journaled — nothing was applied.
    Duplicate {
        /// The idempotency key that was already present (correlation).
        idempotency_key: String,
    },
    /// A claimed workpiece was already held by an active bloom; the whole
    /// commit rolled back and applied nothing (all-or-nothing admission).
    Conflict {
        /// The idempotency key of the refused event (correlation).
        idempotency_key: String,
        /// The first conflicting workpiece.
        workpiece: String,
    },
    /// The commit failed for a non-conflict reason.
    Err {
        /// The idempotency key of the failed event (correlation).
        idempotency_key: String,
        /// A human-readable failure reason.
        error: String,
    },
}

/// Read the whole journal, in sequence order — the recovery replay source
/// (ADR-0149 §Migration step 1). The control actor sends this from `wire` at
/// boot; its reply rebuilds the snapshot.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.replay_journal")]
pub struct ReplayJournal;

/// One journaled event, in the [`ReplayJournalResult`] stream.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// The event's journal sequence.
    pub sequence: u64,
    /// The event's idempotency key.
    pub idempotency_key: String,
    /// The event's canonical `aether_data::wire` bytes.
    pub event: Vec<u8>,
}

/// Reply to [`ReplayJournal`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.replay_journal_result")]
pub enum ReplayJournalResult {
    /// The journal, in sequence order.
    Ok {
        /// Every journaled event, oldest first.
        records: Vec<JournalRecord>,
    },
    /// The replay failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Admit one event to the control loop (ADR-0149 §The control core). The
/// `aether.bloomery.admit` ingress carries an [`Event`](crate::reduce::Event)
/// as canonical `aether_data::wire` bytes (the opaque-bytes convention the
/// store family uses), so an external RPC client or a peer capability admits a
/// fact without the control actor sharing its typed value vocabulary over the
/// wire.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.admit")]
pub struct Admit {
    /// The event's canonical `aether_data::wire` bytes (an encoded
    /// [`Event`](crate::reduce::Event)).
    pub event: Vec<u8>,
}

/// Reply to [`Admit`]: the reducer [`Outcome`](crate::reduce::Outcome) the
/// event resolved to, as canonical `aether_data::wire` bytes. A caller decodes
/// it back into an `Outcome` to learn whether the fact sealed, integrated,
/// landed, or was refused (and why).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.admit_result")]
pub enum AdmitResult {
    /// The event reduced (and durably committed) to an outcome; `outcome` is
    /// the wire-encoded [`Outcome`](crate::reduce::Outcome).
    Ok {
        /// The wire-encoded reducer outcome.
        outcome: Vec<u8>,
    },
    /// The admitted bytes did not decode into an [`Event`](crate::reduce::Event),
    /// or the durable commit failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Read the live projection (ADR-0149 §The boundary). The control actor is the
/// single owner of the live [`Snapshot`](crate::reduce::Snapshot), so reads
/// come from here rather than rebuilding a snapshot per request. With `bloom`
/// unset the reply carries the whole [`ViewDocument`](crate::port::ViewDocument);
/// with `bloom` set to a bloom-id's digest bytes it carries that one bloom's
/// [`BloomView`](crate::port::BloomView).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.query")]
pub struct Query {
    /// The bloom to read, as its digest bytes; unset reads the whole document.
    pub bloom: Option<Vec<u8>>,
}

/// Reply to [`Query`]: the requested projection as canonical
/// `aether_data::wire` bytes — a [`ViewDocument`](crate::port::ViewDocument)
/// for a whole-document read, or a [`BloomView`](crate::port::BloomView) for a
/// single-bloom read.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.query_result")]
pub enum QueryResult {
    /// The whole live projection, wire-encoded
    /// [`ViewDocument`](crate::port::ViewDocument).
    Document {
        /// The wire-encoded `ViewDocument`.
        document: Vec<u8>,
    },
    /// One bloom's view, wire-encoded [`BloomView`](crate::port::BloomView).
    Bloom {
        /// The wire-encoded `BloomView`.
        view: Vec<u8>,
    },
    /// No bloom with the requested id is known.
    NotFound,
}

#[cfg(feature = "runtime")]
mod actor;
#[cfg(feature = "runtime")]
pub use actor::ControlCore;
