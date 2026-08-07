//! Dispatch-record persistence: the outstanding-order registry write side.

use std::error::Error;
use std::fmt;

use aether_bloomery::{BloomId, Digest, Nonce, StageId, Transformation, WorkHandle, WorkOrder, WorkpieceId};
use aether_bloomery_github::{ExecutorError, GithubError};
use aether_data::wire::to_vec;

use crate::bloomery::executor::{ExecutorPortError, ExecutorShell};
use crate::store::{OutstandingOrder, RecordOutcome, StoreBackend};

/// A work order's reducer context, captured host-side at dispatch time — the
/// typed form of an [`OutstandingOrder`] registry
/// row. The caller (the reducer's dispatch path in production, a test here)
/// supplies every field; the portable core [`WorkOrder`] is unchanged.
///
/// A well-formed record has `candidate == displayed_digest`: the digest Bloomery
/// displayed for the order *is* the candidate the worker's evidence must bind to
/// (`Evidence.subject` binds to the candidate). The registry keeps both fields
/// so the broker binds evidence to the displayed digest while the claim names
/// the candidate; the reducer's re-check is what enforces they agree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DispatchRecord {
    /// The dispatched worker's idempotency nonce.
    pub nonce: Nonce,
    /// The bloom the resolved candidate integrates into.
    pub bloom: BloomId,
    /// The member workpiece this order resolves.
    pub workpiece: WorkpieceId,
    /// The scope revision the candidate was integrated against.
    pub scope_revision: Digest,
    /// The exact candidate digest the evidence must bind to.
    pub candidate: Digest,
    /// The digest Bloomery displayed for this order.
    pub displayed_digest: Digest,
    /// The line stage this order dispatched (#3505). Routes the returning result:
    /// a non-terminal per-member stage (`Construct` / `Verify` / `Refine`) admits
    /// as a `Fact::AttemptCompleted` advancing the member's cursor; the terminal
    /// `Review` admits as a `Fact::Integrate`; a parked outcome as a `Question`.
    pub stage: StageId,
    /// The transformation this order dispatched — the record's half of the
    /// [`WorkOrder`] the executor receives, and the exact lane a parked attempt
    /// is re-dispatched by replaying (#3664).
    pub transformation: Transformation,
}

impl DispatchRecord {
    /// The [`WorkOrder`] this record dispatches: the record *is* the order plus
    /// its reducer context, so the two cannot name different nonces or lanes.
    #[must_use]
    pub fn to_order(&self) -> WorkOrder {
        WorkOrder { transformation: self.transformation.clone(), nonce: self.nonce.clone() }
    }

    fn to_stored(&self) -> OutstandingOrder {
        OutstandingOrder {
            nonce: self.nonce.0.clone(),
            bloom: self.bloom.0.as_bytes().to_vec(),
            workpiece: self.workpiece.0.clone(),
            scope_revision: self.scope_revision.as_bytes().to_vec(),
            candidate: self.candidate.as_bytes().to_vec(),
            displayed_digest: self.displayed_digest.as_bytes().to_vec(),
            // The StageId as its canonical wire bytes — a stable, compact column
            // the intake decodes back on admit (never a hand-rolled int mapping).
            stage: to_vec(&self.stage).unwrap_or_default(),
            // Same convention for the transformation, so a parked attempt's lane
            // survives into `parked_question` for the redispatch to replay.
            transformation: to_vec(&self.transformation).unwrap_or_default(),
        }
    }
}

/// Record an outstanding order's reducer context at dispatch time — the
/// registry write side (#3502). Idempotent on the nonce.
///
/// # Errors
/// The durable store faulted.
pub fn record_dispatch(store: &mut dyn StoreBackend, record: &DispatchRecord) -> rusqlite::Result<RecordOutcome> {
    store.record_order(&record.to_stored())
}

/// A dispatch that both submitted and recorded its context, or the step that
/// failed.
#[derive(Debug)]
pub enum DispatchError {
    /// The executor refused or could not reach the dispatch surface.
    Submit(ExecutorPortError),
    /// The registry write faulted after a successful submit.
    Store(rusqlite::Error),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Submit(error) => write!(f, "work-order submit failed: {error}"),
            Self::Store(error) => write!(f, "dispatch-record write failed: {error}"),
        }
    }
}

impl Error for DispatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Submit(error) => Some(error),
            Self::Store(error) => Some(error),
        }
    }
}

impl DispatchError {
    /// Whether this fault is permanent — a GitHub HTTP refusal that will not
    /// clear on retry (a 4xx other than the 429 rate-limit) — as opposed to a
    /// transient fault that a re-drive can recover from: a 429, any 5xx,
    /// transport/decode/pagination faults, `NoRunForNonce`, the whole local-lane
    /// arm (worktree/spawn/io/evidence, never HTTP), and a post-submit registry
    /// write fault.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::Submit(ExecutorPortError::Actions(ExecutorError::Github(GithubError::Status { status, .. })))
                if (400..500).contains(status) && *status != 429
        )
    }
}

/// Submit a work order through the executor shell and record its outstanding
/// reducer context, in one host step (#3502). Submits first, so a submit that
/// fails records nothing; the registry row is written only for a dispatch that
/// actually reached the worker lane. A record write that faults *after* a
/// successful submit best-effort cancels the just-submitted run before
/// propagating — the order would otherwise run untracked, with no registry row
/// to resolve its evidence against.
///
/// # Errors
/// [`DispatchError::Submit`] if the executor refused the dispatch, or
/// [`DispatchError::Store`] if the registry write faulted after submit (the
/// submitted run is best-effort cancelled first).
pub fn dispatch_and_record(
    shell: &ExecutorShell,
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
) -> Result<WorkHandle, DispatchError> {
    let handle = shell.submit(&record.to_order()).map_err(DispatchError::Submit)?;
    if let Err(store_error) = record_dispatch(store, record) {
        // The order reached the worker lane but its reducer context never
        // landed, so it is untracked either way; best-effort cancel the run
        // rather than leak an untracked dispatch, then surface the store fault.
        let _ = shell.cancel(&handle);
        return Err(DispatchError::Store(store_error));
    }
    Ok(handle)
}
