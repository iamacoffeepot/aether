//! Stream-bound admission and pending peer-evaluation tracking.
//!
//! [`Cluster`] owns a views [`Owner`], binds one caller-supplied stream token,
//! and records outstanding live evaluations. It does not execute programs or
//! persist a checkpoint.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;

use aether_bloomery_kinds::Seq;
use aether_data::MailboxId;

use crate::bundle::{ClusterStatus, EvaluatedResult, PeerEvaluated};
use crate::error::PrepareError;
use crate::owner::Owner;

/// Running views-owner cluster: one stream binding, one retained prefix.
pub struct Cluster {
    owner: Owner,
    stream: Option<String>,
    poisoned: bool,
    last_trusted_cursor: Seq,
    pending: BTreeMap<u64, Pending>,
}

struct Pending {
    expected: BTreeSet<MailboxId>,
    failed: bool,
    stream: String,
    message: String,
}

impl Cluster {
    /// Empty prefix, unbound stream, no pending evaluations.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            owner: Owner::new(),
            stream: None,
            poisoned: false,
            last_trusted_cursor: Seq(0),
            pending: BTreeMap::new(),
        }
    }

    /// Retained views owner.
    #[must_use]
    pub const fn owner(&self) -> &Owner {
        &self.owner
    }

    /// Mutable retained views owner.
    pub fn owner_mut(&mut self) -> &mut Owner {
        &mut self.owner
    }

    /// Aggregation cursor, bound stream, and poison flag. Not evaluation.
    #[must_use]
    pub fn status(&self) -> ClusterStatus {
        ClusterStatus {
            cursor: self.owner.cursor().0,
            stream: self.stream.clone().unwrap_or_default(),
            poisoned: self.is_poisoned(),
        }
    }

    /// Whether a fold failed or a constructed view is unusable.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned || self.owner.is_poisoned()
    }

    /// Bound stream token, if a successful admit has occurred.
    #[must_use]
    pub fn stream(&self) -> Option<&str> {
        self.stream.as_deref()
    }

    /// Refuse empty tokens, mismatches, and further input after a failed fold.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when this cluster cannot admit `stream`.
    pub fn check_admit(&self, stream: &str) -> Result<(), PrepareError> {
        if stream.is_empty() {
            return Err(PrepareError::InvalidStream);
        }
        if self.is_poisoned() {
            return Err(PrepareError::PoisonedCluster { last_trusted_cursor: self.last_trusted_cursor });
        }
        if let Some(bound) = &self.stream
            && bound != stream
        {
            return Err(PrepareError::StreamMismatch { bound: bound.clone(), actual: String::from(stream) });
        }
        Ok(())
    }

    /// Remember `stream` after a successful admit. No-op when already bound.
    pub fn bind_stream(&mut self, stream: impl Into<String>) {
        if self.stream.is_none() {
            self.stream = Some(stream.into());
        }
    }

    /// Record that a fold failed. Further admission is refused until rebuild.
    pub fn mark_poisoned(&mut self) {
        self.poisoned = true;
    }

    /// Record the last cursor trusted after a successful fold.
    pub fn trust_cursor(&mut self) {
        self.last_trusted_cursor = self.owner.cursor();
    }

    /// Expect one ordinary-mail outcome from each `peers` mailbox for live `seq`.
    ///
    /// Returns [`Some`] immediately when `peers` is empty so a cluster with no
    /// reachable peers cannot emit a successful evaluation. Duplicate mailbox
    /// ids collapse to one expected reporter.
    pub fn start_live(&mut self, stream: impl Into<String>, seq: u64, peers: &[MailboxId]) -> Option<EvaluatedResult> {
        let stream = stream.into();
        let expected: BTreeSet<MailboxId> = peers.iter().copied().collect();
        if expected.is_empty() {
            return Some(EvaluatedResult::from_error(stream, seq, "reactor cluster has no reachable peers"));
        }
        self.pending.insert(seq, Pending { expected, failed: false, stream, message: String::new() });
        None
    }

    /// Apply one peer outcome from the engine-stamped `source` mailbox.
    ///
    /// Unknown, duplicate, outdated, or wrong-stream reports are ignored and
    /// cannot complete evaluation. Each expected mailbox is consumed once.
    pub fn note_peer(&mut self, source: MailboxId, outcome: &PeerEvaluated) -> Option<EvaluatedResult> {
        let pending = self.pending.get_mut(&outcome.seq())?;
        if pending.stream != outcome.stream() {
            return None;
        }
        if !pending.expected.remove(&source) {
            return None;
        }
        if !outcome.is_ok() {
            pending.failed = true;
            if let PeerEvaluated::Err { message, .. } = outcome
                && pending.message.is_empty()
            {
                pending.message.clone_from(message);
            }
        }
        if !pending.expected.is_empty() {
            return None;
        }
        let finished = self.pending.remove(&outcome.seq())?;
        if finished.failed {
            let message = if finished.message.is_empty() {
                String::from("reactor peer evaluation failed")
            } else {
                finished.message
            };
            Some(EvaluatedResult::from_error(finished.stream, outcome.seq(), message))
        } else {
            Some(EvaluatedResult::ok(finished.stream, outcome.seq()))
        }
    }
}

impl Default for Cluster {
    fn default() -> Self {
        Self::new()
    }
}
