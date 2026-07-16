//! The canonical value vocabulary (ADR-0149 §The value vocabulary).
//!
//! Everything durable is an immutable [`Artifact`], typed and content
//! addressed by [`Digest`], forming a derivation DAG in which
//! every artifact names its parents. The types here are the vocabulary that
//! DAG is built from; they are plain data — no I/O, no engine boot, no
//! GitHub types — and are content-addressed the same way (`digest_of`).

mod bloom;
mod profile;
mod scope_revision;
mod stage;
mod statement;
mod study;

pub use bloom::{BloomDraft, BloomSpec, LandingReceipt, Membership, ResolutionClaim, ResolvedBloom};
pub use profile::{AgentProfile, ReasoningEffort, ToolPolicy};
pub use scope_revision::{ModelOverride, ResolvedModel, ScopeRevision};
pub use stage::{Attempt, NetworkProfile, StageBinding, StageCatalog, Transformation};
pub use statement::{Observation, Provenance, StageReceipt, Statement};
pub use study::{StudyCost, StudyRecord};

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::WorkpieceId;

/// The generic immutable node of the derivation DAG: opaque typed bytes plus
/// the digests of the parents it derives from. Every durable value projects
/// to an artifact; the artifact's identity is `digest_of(self)`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Artifact {
    /// The typed tag naming what kind of bytes these are.
    pub media_type: String,
    /// The content bytes.
    pub bytes: Vec<u8>,
    /// The parents this artifact derives from, named by digest.
    pub parents: Vec<Digest>,
}

impl ContentAddressed for Artifact {
    const DOMAIN: &'static str = "aether.bloomery.artifact";
}

impl Artifact {
    /// The artifact's content-addressed identity.
    #[must_use]
    pub fn id(&self) -> Digest {
        digest_of(self)
    }
}

/// The stable identity of one intended change plus its current scope
/// revision. A GitHub issue is one projection of a workpiece; the workpiece
/// is the native truth (ADR-0149 §The bloom).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Workpiece {
    /// The stable, projection-independent identity.
    pub id: WorkpieceId,
    /// The statement digest expressing the change's intent.
    pub intent: Digest,
    /// The current scope-revision digest — a sealed bloom pins the exact
    /// revision it admitted, so a later revision is a new candidate.
    pub scope_revision: Digest,
}

/// Approvals, verification results, review findings, and resolution claims
/// bind to *exact* digests: refinement produces a new candidate and old
/// evidence never validates the replacement (ADR-0149 §The value
/// vocabulary).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Evidence {
    /// The exact digest this evidence attests to. Evidence for one digest
    /// says nothing about any other.
    pub subject: Digest,
    /// What the evidence asserts about its subject.
    pub kind: EvidenceKind,
    /// The supporting artifact (the check output, the review record, …).
    pub detail: Digest,
}

impl Evidence {
    /// Does this evidence attest to `subject`? True only for the exact
    /// digest it names — the "no evidence validates a digest it does not
    /// name" invariant, at the type level.
    #[must_use]
    pub fn validates(&self, subject: &Digest) -> bool {
        self.subject == *subject
    }
}

/// The class of an [`Evidence`] artifact.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum EvidenceKind {
    /// An owner (or policy) approval of a scope revision.
    Approval,
    /// A verification stage's pass/fail result.
    VerificationResult,
    /// A review finding.
    ReviewFinding,
    /// A per-member resolution claim on the final tree.
    ResolutionClaim,
    /// A normalized per-attempt cost record — the study evidence a forecast
    /// grade reads (ADR-0151). Its `detail` names a [`StudyRecord`] artifact.
    StudyRecord,
}

/// A sealed bloom's predetermined resource ceiling (ADR-0149 §The bloom):
/// the bounded promise study grades actual against.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Budget {
    /// The token ceiling for the whole bloom.
    pub token_ceiling: u64,
    /// The wall-clock ceiling in seconds.
    pub wall_clock_secs: u64,
    /// The per-stage retry cap.
    pub retry_cap: u32,
}

/// The sealed forecast of what a bloom's set will cost — what a study report
/// grades actual cost, time, and retries against after landing (ADR-0149
/// §The bloom).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Forecast {
    /// The predicted total cost (tokens).
    pub predicted_cost: u64,
    /// The predicted wall-clock time in seconds.
    pub predicted_secs: u64,
    /// The predicted number of stage retries — the retry axis of the grade
    /// (ADR-0151; `Budget::retry_cap` precedents the shape).
    pub predicted_retries: u32,
}
