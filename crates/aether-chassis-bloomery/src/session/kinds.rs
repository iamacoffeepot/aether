//! The `aether.session.*` mail vocabulary (executor session-reuse pool).
//!
//! The runner lane (#3511) drives these on the `"aether.session"` mailbox to
//! pool a headless-Claude session across a construct/verify/refine attempt:
//! `acquire` leases an eligible pooled session so the next attempt resumes it
//! (reusing the prompt-cached static prefix) rather than launching cold, and
//! `release` deposits a session back for the next attempt to find. Each request
//! is paired 1:1 with an `Ok`/`Err`-shaped reply, following the store/artifacts
//! reply-enum precedent.
//!
//! Digests are hex `String`s throughout — the exact form `aether.artifacts.put`
//! hands back (`PutResult::Ok { digest: String }`), so the runner threads a
//! `put` result straight into `release` and an `acquire` reply straight into
//! `aether.artifacts.get` with no conversion. The pool holds these digests plus
//! the eligibility metadata and the lease; the session transcript bytes live in
//! `aether.artifacts` (content-addressed, eviction-free), never in the pool.
//!
//! Always-on (no `cfg` gate): a peer that addresses the cap via
//! `ctx.actor::<SessionPoolCapability>()` needs these types on the
//! target-agnostic build, so the whole family lives here rather than behind the
//! `runtime` feature.

use serde::{Deserialize, Serialize};

/// The pool *identity* of a session: the axes on which a resume must match to
/// reuse the prompt cache. `model` and `effort` are **both** key axes — an
/// effort flip on resume breaks the prompt cache the same way a model flip does
/// (#3264) — and `task` scopes a pooled session to its stage family. This is the
/// full key; `head_hash` is an `acquire`-time freshness input (not a key axis),
/// and `workspace_tree_hash` is not on the key at all (audit-only, #3341).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionKey {
    /// The CLI model id the session ran under (e.g. `claude-opus-4-8`).
    pub model: String,
    /// The reasoning-effort tier the session ran at (e.g. `high`).
    pub effort: String,
    /// The pipeline task the session serves (e.g. `implement`).
    pub task: String,
}

/// An exclusive lease over a pooled session, issued by [`Acquire`] and echoed
/// back on [`Release`]. Opaque to the caller — the pool derives it from the row
/// it leased. Lease ownership is not hard-validated on release (a crashed
/// holder is reclaimed by lazy expiry at the next `acquire`, so a `release` is
/// an unconditional deposit), so the token is a provenance breadcrumb, not a
/// capability check.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LeaseToken(pub String);

/// The provenance + eligibility metadata a [`Release`] deposits with a session.
///
/// `head_hash` and `context_tokens` are the two `acquire`-time eligibility
/// inputs the pool gates on (head-freshness #3422, context cap). `receipt` is
/// this session's own `Provenance::StageReceipt` `Statement` digest — the
/// content-addressed attestation of the stage the session produced — and
/// `parent_receipt` is the *prior* session's `receipt` (the one [`Acquire`]
/// handed back), so a chain of resumes is provenance-linked: ADR-0149's
/// fail-closed closure applied to the resume case, even though the resumed
/// context is not re-sent in the new prompt manifest. `workspace_tree_hash` and
/// `read_files` are carried for audit/provenance only and are **never** gated —
/// #3341 retired belief-truth subtree matching because a resume re-derives every
/// deciding fact on the fresh checkout, and this pool's sole consumer is the
/// construct/verify/refine loop where the workpiece tree changing between
/// attempts is the point.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionManifest {
    /// The prior session's [`receipt`](Self::receipt), or `None` for a cold
    /// session with no resume ancestor.
    pub parent_receipt: Option<String>,
    /// This session's own `Provenance::StageReceipt` `Statement` digest — the
    /// value `acquire` hands the next resume as its `parent_receipt`.
    pub receipt: String,
    /// The static-prefix (`CLAUDE.md` + skill text) head hash at deposit time —
    /// the #3422 head-freshness eligibility input.
    pub head_hash: String,
    /// The session's terminal context size — the context-cap eligibility input.
    pub context_tokens: u64,
    /// The workpiece tree hash at deposit time — audit-only, never gated (#3341).
    pub workspace_tree_hash: String,
    /// The cumulative main-loop read set — audit-only.
    pub read_files: Vec<String>,
    /// Deposit time, seconds since the Unix epoch — the age-bound input, stamped
    /// by the depositing caller (the fleet shares one clock domain).
    pub deposited_at: u64,
}

/// `aether.session.acquire` — lease an eligible pooled session for `key`, or
/// report that none exists so the runner launches cold. `current_head_hash` is
/// the resuming box's *current* static-prefix hash; the pool gates reuse on it
/// matching the deposited session's `head_hash` (#3422), so a head that moved on
/// `origin/main` between deposit and resume is a real cache miss. Reply:
/// [`AcquireResult`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.session.acquire")]
pub struct Acquire {
    /// The `{model, effort, task}` identity to match.
    pub key: SessionKey,
    /// The resuming box's current static-prefix head hash (#3422 freshness gate).
    pub current_head_hash: String,
}

/// Reply to [`Acquire`]. `Leased` carries the exclusive lease plus the session
/// transcript digest to resume and the acquired session's `receipt` (which the
/// resuming attempt records as its `parent_receipt` on the eventual
/// [`Release`]); `None` means no eligible session exists and the runner starts
/// cold; `Err` carries a backend failure.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.session.acquire_result")]
pub enum AcquireResult {
    /// An eligible session was leased.
    Leased {
        /// The exclusive lease over the leased row.
        lease: LeaseToken,
        /// The session transcript's `aether.artifacts` digest, to resume.
        session_bytes: String,
        /// The acquired session's own receipt — the resumed attempt's parent.
        parent_receipt: String,
    },
    /// No eligible pooled session exists for `key` — launch cold.
    None,
    /// The acquire failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// `aether.session.release` — deposit `session_bytes` back into the pool for
/// `key`, unleased, carrying its `manifest`. Upserts the one pooled session per
/// key: a warm release (with the `lease` `acquire` issued) updates the row a
/// resume leased, and a cold release (`lease` = `None`) inserts a fresh row the
/// runner wants to pool. Reply: [`ReleaseResult`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.session.release")]
pub struct Release {
    /// The `{model, effort, task}` identity to deposit under.
    pub key: SessionKey,
    /// The lease `acquire` issued, or `None` for a cold deposit.
    pub lease: Option<LeaseToken>,
    /// The session transcript's `aether.artifacts` digest.
    pub session_bytes: String,
    /// The deposited session's provenance + eligibility metadata.
    pub manifest: SessionManifest,
}

/// Reply to [`Release`]. `Ok` when the session was deposited (inserted or
/// updated) unleased; `Err` carries a backend failure.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.session.release_result")]
pub enum ReleaseResult {
    /// The session was deposited into the pool, unleased.
    Ok,
    /// The presented lease is not the one the pooled row holds, so nothing was
    /// deposited — a stale holder returning after its lease expired and was
    /// re-acquired (#3665). Distinct from `Err`: the store worked.
    NotLeaseHolder,
    /// The release failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}
