//! Immutable aggregate pre-check plans and their journal-replayable state.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{BloomId, WorkpieceId};
use crate::values::{CandidateRef, Evidence};

/// Optional bloom-wide policy for speculative aggregate verification.
#[aether_data::kind(name = "aether.bloomery.precheck_policy", default, eq)]
pub struct PrecheckPolicy {
    /// Maximum number of physical speculative runs the bloom may issue.
    pub run_budget: u32,
}

/// One immutable member input in sealed member order.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PrecheckMember {
    /// The member whose captured candidate participates.
    pub workpiece: WorkpieceId,
    /// The frozen scope revision the candidate implements.
    pub scope_revision: Digest,
    /// Both the candidate tree identity and its checkout vehicle.
    pub candidate: CandidateRef,
}

/// A content-addressed request to fold captured candidates in scratch space.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PrecheckPlan {
    /// The sealed bloom this preview belongs to.
    pub bloom: BloomId,
    /// The immutable scratch-fold base for legacy plans, or the already
    /// materialized selected head's checkout for eager plans. The aggregate
    /// invocation independently retains the sealed bloom base as its diff base.
    pub base: Digest,
    /// Eligible candidates in sealed membership order for a legacy fold, or
    /// the selected eager head's actual member coverage order.
    pub members: Vec<PrecheckMember>,
    /// The sealed aggregate-verify gate-set identity.
    pub gate_set: Digest,
}

impl ContentAddressed for PrecheckPlan {
    const DOMAIN: &'static str = "aether.bloomery.precheck_plan";
}

impl PrecheckPlan {
    /// The plan's stable identity. Candidate versions, not arrival order, decide it.
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// The immutable output of preparing one pre-check plan.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PrecheckNode {
    /// The exact plan this node prepared.
    pub plan: Digest,
    /// The scratch fold's resulting tree and verification subject.
    pub tree: Digest,
    /// The checkout commit carrying `tree`.
    pub head: Digest,
    /// The aggregate gate set the eventual run executes.
    pub gate_set: Digest,
}

impl ContentAddressed for PrecheckNode {
    const DOMAIN: &'static str = "aether.bloomery.precheck_node";
}

impl PrecheckNode {
    /// The node's stable identity.
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Host preparation result for the latest plan.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PrecheckPreparation {
    /// Scratch folding produced an immutable runnable node.
    Prepared(PrecheckNode),
    /// Scratch folding refused, with a durable diagnostic artifact.
    Refused { detail: Digest },
}

/// Host completion for one issued node.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PrecheckCompletion {
    /// The aggregate mechanical gate passed.
    Passed(Evidence),
    /// The aggregate mechanical gate reached a red verdict.
    Failed(Evidence),
    /// The host could not complete the requested run.
    HostFault(Evidence),
    /// The idle-only offer became obsolete before a worker started it.
    SkippedBeforeStart,
}

/// The latest operator-visible pre-check diagnostic.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PrecheckDiagnostic {
    /// Scratch folding refused the plan.
    PreparationRefused { plan: Digest, detail: Digest },
    /// The current issued node failed its mechanical gate.
    VerificationFailed { node: Digest, detail: Digest },
    /// The current issued node hit a host fault.
    HostFault { node: Digest, detail: Digest },
}

/// The recorded terminal result of the current issued node.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PrecheckResult {
    Passed { node: Digest, evidence: Digest },
    Failed { node: Digest, evidence: Digest },
    HostFault { node: Digest, evidence: Digest },
    SkippedBeforeStart { node: Digest },
}

/// Complete reducer-owned state needed to replay pre-check scheduling.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PrecheckState {
    /// The sealed policy that enabled this bloom.
    pub policy: PrecheckPolicy,
    /// Newest content-addressed candidate plan.
    pub latest_plan: Option<PrecheckPlan>,
    /// Newest successfully prepared node.
    pub prepared: Option<PrecheckNode>,
    /// The full immutable input of the single issued physical run.
    pub issued: Option<PrecheckNode>,
    /// Physical runs issued so far; skipped-before-start runs are reclaimed.
    pub issued_runs: u32,
    /// Current node's terminal result, when one exists.
    pub result: Option<PrecheckResult>,
    /// Current composition-only diagnostic.
    pub diagnostic: Option<PrecheckDiagnostic>,
    /// Issued node joined by final resolution, if any.
    pub final_join: Option<PrecheckNode>,
    /// Whether the joined idle run has been promoted to required execution.
    pub promoted: bool,
    /// Whether an operator hold currently pauses idle admission.
    pub paused: bool,
}

impl PrecheckState {
    #[must_use]
    pub fn new(policy: PrecheckPolicy) -> Self {
        Self {
            policy,
            latest_plan: None,
            prepared: None,
            issued: None,
            issued_runs: 0,
            result: None,
            diagnostic: None,
            final_join: None,
            promoted: false,
            paused: false,
        }
    }

    #[must_use]
    pub fn is_current_plan(&self, plan: Digest) -> bool {
        self.latest_plan.as_ref().is_some_and(|current| current.digest() == plan)
    }

    #[must_use]
    pub fn is_current_node(&self, node: Digest) -> bool {
        self.prepared.as_ref().is_some_and(|current| current.digest() == node)
    }

    #[must_use]
    pub fn can_request(&self, node: Digest) -> bool {
        !self.paused
            && self.is_current_node(node)
            && self.issued.is_none()
            && self.remaining_runs() > 0
            && matches!(
                self.result.as_ref(),
                None | Some(PrecheckResult::HostFault { .. } | PrecheckResult::SkippedBeforeStart { .. })
            )
    }

    #[must_use]
    pub fn issued_is(&self, node: Digest) -> bool {
        self.issued.as_ref().is_some_and(|issued| issued.digest() == node)
    }

    #[must_use]
    pub fn joined_is(&self, node: Digest) -> bool {
        self.final_join.as_ref().is_some_and(|joined| joined.digest() == node)
    }

    #[must_use]
    pub fn remaining_runs(&self) -> u32 {
        self.policy.run_budget.saturating_sub(self.issued_runs)
    }
}
