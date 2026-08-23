//! The canonical value vocabulary (ADR-0149 §The value vocabulary).
//!
//! Everything durable is an immutable [`Artifact`], typed and content
//! addressed by [`Digest`], forming a derivation DAG in which
//! every artifact names its parents. The types here are the vocabulary that
//! DAG is built from; they are plain data — no I/O, no engine boot, no
//! GitHub types — and are content-addressed the same way (`digest_of`).

mod adr;
mod approval;
mod base_verify;
mod bloom;
mod commission;
mod composition;
mod config;
mod fields;
mod finding;
mod granularity;
mod lane;
mod lease;
mod model_override;
mod narrowing;
mod operator;
mod orphan_claim;
mod price;
mod profile;
mod proof;
mod question;
mod scope_verify;
mod spend;
mod stage;
mod statement;
mod study;
mod suppression;
mod surface;
mod timeout;
mod verify;
mod workpiece_builder;

pub use adr::{ADR_SCHEMA, ADR_TRANSITION_SCHEMA, Adr, AdrStatus, AdrTransition, AdrValueError};
pub use approval::{
    ApprovalPolicy, ApprovalRule, SurfacePattern, Tier, TierVerdict, gate_widening, path_in_surface, surface_additions,
    surface_intersection, surface_union, tier_verdict,
};
pub use base_verify::{BaseReceipt, BaseVerdict};
pub use bloom::{
    BloomDraft, BloomSpec, DependencyError, LandingReceipt, MemberCandidate, MemberDependency, MemberSubject,
    Membership, ResolutionClaim, ResolvedBloom, ResolvedDependencies, resolve_member_dependencies,
};
pub use commission::{
    CommissionApprovalTier, CommissionStatementRole, CommissionStatus, CommissionValueError, SCOPE_REVISION_SCHEMA,
    ScopeRevision, ScopeRouting,
};
pub use composition::CompositionFinding;
pub use config::{
    ConfigKind, ConfigRegistry, ConfigResolveError, ConfigScopes, ResolvedConfigs, Unproducible, config_address,
    decode_config,
};
pub use fields::{FieldKind, WorkpieceFact, WorkpieceFields};
pub use finding::{
    CHECK_KEY, CRITICAL_KEY, ClassifiedFinding, ClassifiedFindings, FindingClass, JUDGMENT_TAG, MECHANICAL_TAG,
    classify_findings,
};
pub use granularity::{Widening, coarsen, rewrites, widen};
pub use lane::{LANE_WORKPIECE_HEADER, pin_workpiece_description, split_lane_identity};
pub use lease::{EvictedHolder, MAX_OBSERVED_WRITES, normalize_write_paths};
pub use model_override::{AgentSelection, ModelOverride, OverrideError, ResolvedModel, StageOverride};
pub use narrowing::{CompositionParents, FoldContribution, NarrowingRefusal, narrow_composition};
pub use operator::{Adjudication, Disposition, OperatorHold, OperatorRepair, Withdrawal, WithdrawalCause};
pub use orphan_claim::{
    ORPHAN_CLAIM_RELEASE_WORDS, OrphanClaimRelease, OrphanClaimReleaseCompletion, OrphanClaimReleaseRecord,
};
pub use price::{LongContextBand, PriceRates, PriceTable, SealedPriceTable};
pub use profile::{AgentProfile, Harness, ReasoningEffort, ToolPolicy};
pub use proof::{VerifiedTree, VerifyGateSet, VerifyProof, VerifyReuse};
pub use question::Question;
pub use scope_verify::{
    NamedPath, NamedSymbol, PathOrigin, SCOPE_VERIFY_SCHEMA, ScopeVerifyInput, ScopeVerifyReport, verify_scope,
};
pub use spend::{SpendCeiling, SpendQuiesce, SpendWindow};
pub use stage::{
    Attempt, CONSTRUCT_IMPLEMENT_COMMAND, CandidateRef, CatalogError, DispatchKey, ExecutionLimits, NetworkProfile,
    REVIEW_CRITIC_COMMAND, SCOPE_FILL_COMMAND, StageBinding, StageCatalog, Transformation, VERIFY_BASE_COMMAND,
    VERIFY_CHECK_COMMAND, VERIFY_LANE_IMAGE, VERIFY_LANE_NETWORK, is_model_lane,
};
pub use statement::{Observation, Provenance, StageReceipt, Statement};
#[cfg(not(target_arch = "wasm32"))]
pub use statement::{signed_approval, signed_cancel, signed_reopen};
pub use study::{StudyCall, StudyCost, StudyRecord};
pub use suppression::{SuppressionDisposition, SuppressionRequest, SuppressionVerdict};
pub use surface::{SurfacePathRequest, SurfaceRequest};
pub use timeout::TimeoutRecord;
pub use verify::{VerifyFailure, VerifyFailureSet};
pub use workpiece_builder::{FIELD_ENTRY_SCHEMA, FieldEntry, WorkpieceBuilder, WorkpieceRefusal};

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{StageId, WorkpieceId};

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
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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

/// Why a member stopped dispatching for good (ADR-0149 §The line).
///
/// A member that exhausts a stage's `retry_budget` wedges: the reducer stops
/// dispatching it deliberately — never an extra roll, never a silent integrate.
/// An explicit attempt grant or a successor cursor can re-enter the line.
/// Wedging is recorded rather than merely decided: the stage cursor cannot
/// express it (a member sitting at `Verify` one roll below the ceiling is
/// mid-flight and looks identical), and without a record the outward view
/// reports a wedged member exactly as it reports a working one.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Wedge {
    /// The stage whose retry budget the member exhausted.
    pub stage: StageId,
    /// The artifact of the failure that consumed the last of the budget — the
    /// `detail` of the evidence the wedging attempt returned, so a reader can
    /// go straight to what went wrong rather than replaying the line.
    pub evidence: Digest,
    /// The verifier identities from the terminal verdict that this member had
    /// already failed before. Nonempty only when repeated Verify failures spent
    /// the terminal repair roll; every other stage wedges with the empty set —
    /// as does a `Verify` that exhausted its attempts on verdicts naming no
    /// verifier at all, the shape a gate that never answered leaves behind.
    pub repeated_verifiers: VerifyFailureSet,
}

/// The class of an [`Evidence`] artifact.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
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
    /// A parked attempt's decision request — evidence *about* the attempt, never
    /// intent (ADR-0151). Its `detail` names a [`Question`] artifact, and
    /// admitting it folds a per-member pending-decision hold that blocks the
    /// bloom from resolving until an adopting answer releases it.
    Question,
    /// A dispatched lane's report that it could not judge its subject at all
    /// (ADR-0176) — an executor environment fault, evidence *about* the
    /// dispatch rather than about the candidate.
    ///
    /// Its `detail` names the fault report the lane produced. Admitting it folds
    /// the bloom's aggregate-review fault series (see
    /// [`BloomRecord::aggregate_fault`](crate::BloomRecord::aggregate_fault)),
    /// keyed to the subject it names, the way a
    /// [`Question`](Self::Question) admission folds a pending-decision hold.
    /// Appended past [`EvidenceKind::Question`] so the prior kinds' wire
    /// discriminants are unchanged.
    ExecutorFault,
    /// A cross-member fold collision (ADR-0189). Its `detail` names the
    /// conflicting-path report the integrate reactor persisted; admitting it
    /// does not raise a hold. Appended past [`Self::ExecutorFault`] so the
    /// prior kinds' wire discriminants are unchanged.
    FoldConflict,
    /// A repair lap that came back without addressing the finding it was
    /// dispatched for (#4959) — evidence about the *lap*, returned by the host's
    /// mechanical triage rather than by a gate.
    ///
    /// Its `detail` names the lane's own evidence artifact, the same one a
    /// passing lap would have carried; what makes the row countable is the kind.
    /// The lap admits as an ordinary failing repair, so it spends the retry
    /// budget a refused lap spends and wedges the workpiece once that is gone.
    /// Appended past [`Self::FoldConflict`] so the prior kinds' wire
    /// discriminants are unchanged.
    RepairTriage,
    /// A composition review that **passed** while still recording judgment
    /// findings the reviewer did not mark blocking (#4961).
    ///
    /// Its `detail` names the review record the pass was stamped from, exactly
    /// as a [`ReviewFinding`](Self::ReviewFinding) would — what makes the row
    /// distinct is the kind, which is what tells the reducer to file the
    /// advisories on the composition's findings channel on its way to resolving
    /// the bloom. A subjective finding must not break a bloom, and it must not
    /// evaporate either: this is the second half. Appended past
    /// [`Self::RepairTriage`] so the prior kinds' wire discriminants are
    /// unchanged.
    ReviewAdvisory,
    /// A construct lane that concluded cleanly and reported it produced no
    /// candidate (#5292). Riding [`crate::Fact::AttemptCompleted`] so the
    /// reducer can park the member instead of retrying: a dead construct
    /// also captures nothing, and inferring the refusal from that absence
    /// would convert a genuine retry into a park.
    ///
    /// Its `detail` names the lane's evidence artifact — the diagnosis the
    /// operator reads. Appended past [`Self::ReviewAdvisory`] so the prior
    /// kinds' wire discriminants are unchanged.
    ConstructDeclined,
    /// A candidate's stated case for the suppressions it is carrying
    /// (ADR-0193). Its `detail` names the [`SuppressionRequest`] set the lane
    /// wrote on the suppression lines themselves.
    ///
    /// It raises no hold and advances no member — a request is a question
    /// asked of a reviewer who does not exist yet at the moment a member
    /// verifies, and the member proceeds while it stands. Appended past
    /// [`Self::ConstructDeclined`] so the prior kinds' wire discriminants are
    /// unchanged.
    SuppressionRequest,
}

/// The sealed forecast of what a bloom's set will spend — what a study report
/// grades the actuals against after landing (ADR-0149 §The bloom, ADR-0177).
///
/// Graded, never enforced: an overshoot is reported and refuses no dispatch.
/// Each field names the quantity it measures, because none of them is elapsed
/// bloom time — [`predicted_worker_secs`] sums the attempts' own durations, so
/// a bloom running members concurrently accumulates it faster than the clock on
/// the wall.
///
/// [`predicted_worker_secs`]: Self::predicted_worker_secs
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Forecast {
    /// The predicted total token spend, summed over every attempt.
    pub predicted_tokens: u64,
    /// The predicted total worker time in whole seconds — the sum of the
    /// attempts' own durations, not the bloom's elapsed wall-clock time.
    pub predicted_worker_secs: u64,
    /// The predicted number of stage retries — attempts beyond the first.
    pub predicted_retries: u32,
}
