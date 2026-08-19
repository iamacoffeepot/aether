//! aether-bloomery: the canonical value vocabulary and pure control core
//! of Bloomery (ADR-0149).
//!
//! A pure rlib providing the two things the whole control plane rests on: a
//! canonical, immutable value vocabulary that everything durable is
//! expressed in, and the one pure function that owns every state
//! transition.
//!
//! # A `std` crate with an `alloc`-only vocabulary
//!
//! The crate compiles as `std`. Nothing here requires `#![no_std]`: the
//! control core is a native capability rather than a wasm component
//! (ADR-0149 §The boundary), so this crate emits no wasm and carries no
//! guest-SDK dependency, and the native host ([#3458]) and the GitHub
//! adapter link it directly.
//!
//! The vocabulary is nonetheless written against `alloc` paths — `alloc::vec`
//! and friends, never their `std` re-exports — so the types stay portable to
//! a `no_std` consumer without a fork. That is what the `extern crate alloc`
//! below buys; `std` beyond it appears only under `#[cfg(test)]`, since the
//! integration tests in `tests/` are ordinary std crates.
//!
//! # The shape
//!
//! - [`Digest`] / [`digest_of`] — content addressing: a sha256 over a
//!   value's canonical aether-wire bytes ([`aether_data::wire`], ADR-0118).
//! - [`values`] — the immutable value vocabulary forming a derivation DAG
//!   in which every artifact names its parents by digest: [`Artifact`],
//!   [`Statement`] + [`Provenance`], [`Workpiece`], the one-way bloom
//!   lifecycle ([`BloomDraft`] → [`BloomSpec`] → [`ResolvedBloom`] →
//!   [`LandingReceipt`]), [`Evidence`], [`StageBinding`] / [`StageCatalog`]
//!   and the [`AgentProfile`] a binding references, [`Attempt`],
//!   [`Transformation`].
//! - [`sign`] — the [`KeyProvider`] trait and its fake always-valid
//!   implementation. ADR-0149 ships the signature *shapes*; key custody is
//!   a later arc.
//! - [`manifest`] — [`PromptManifest`] assembly, fail-closed: an
//!   instruction-capable slot that does not trace to a signed statement or
//!   a versioned policy artifact rejects the attempt before dispatch.
//! - [`mod@reduce`] — the pure control core: [`reduce`](reduce::reduce) owns
//!   every state transition, with no I/O, no engine boot, no GitHub types.
//! - [`metrics`] — the metrics ledger: [`MetricsLedger`] folds the
//!   journal into dispatch / bloom / day rollups and seat rows. Cost stays a
//!   study-artifact digest; `cost == 0` is unpriced, never free.
//! - [`calibration`] — the capability ledger: [`CalibrationLedger`] folds the
//!   journal into per-`(harness, model, effort) × stage` counts — attempts,
//!   rolls, typed verifier failures, and study-measured cost — so which agent to
//!   run a stage under is an evidenced choice (ADR-0184).
//! - [`study_report`] — the pure forecast grade: [`grade`] folds a bloom's
//!   admitted study records into actual cost and time, reads its retries off the
//!   dispatch ledger, and grades all three against the sealed [`Forecast`]
//!   (ADR-0151, ADR-0180).
//! - [`mod@spend`] — the window spend projection: [`measure`] sums
//!   the priced column on each bloom's admitted study records so the seal-door
//!   governor and the ledger share one accounting path (ADR-0192).
//! - [`port`] — the [`SourceBackend`] / [`ProjectionBackend`] /
//!   [`ExecutorBackend`] trait shapes adapters implement and the host mounts.
//!   Kept here so adapters depend inward on this crate, cycle-free (ADR-0149
//!   §The boundary).
//!
//! [#3458]: https://github.com/iamacoffeepot/aether/issues/3458

// Declared even though the crate is `std`, so the vocabulary's imports name
// `alloc` directly and a `std`-only type cannot reach the value types unnoticed.
// See the crate docs above for why the crate is `std` at all (issue #3497).
extern crate alloc;

pub mod calibration;
pub mod control;
pub mod correspondence;
pub mod digest;
pub mod ids;
pub mod inward;
mod ledger;
pub mod manifest;
pub mod metrics;
pub mod port;
pub mod reduce;
pub mod sign;
pub mod spend;
pub mod study_report;
pub mod values;

pub use calibration::{
    CalibrationDocument, CalibrationLedger, CapabilityCell, CapabilityLedger, LEDGER_CAVEAT, VerifierFailures,
};
pub use control::{
    Admit, AdmitResult, AggregateReviewPayload, AggregateVerifyPayload, CONTROL_CORE_NAMESPACE, ClaimResult, ClaimSeal,
    Commit, CommitResult, CompleteRelease, CompleteReleaseResult, CompleteTransfer, ConfigRecord, DispatchPayload,
    EnumerateClaims, EnumerateClaimsResult, IntegratePayload, JournalRecord, LandPayload, LoadConfigs,
    LoadConfigsResult, MembershipMutation, MetricsQuery, MetricsQueryResult, MetricsView, ObserveMainline,
    ObserveMainlineResult, OrphanClaimReleasePayload, OutboxPayload, Query, QueryResult, RedispatchPayload,
    ReleaseSeal, ReplayJournal, ReplayJournalResult, ReviewPass, SourceReplicaPayload, SpendQuery, SpendQueryResult,
    SplicePayload, Topic, TransferSeal,
};
pub use correspondence::{BackendObjectId, Correspondence, CorrespondenceError, SharedCorrespondence};
pub use digest::{ContentAddressed, Digest, digest_of};
pub use ids::{BloomId, IdempotencyKey, KeyId, Nonce, StageId, WorkpieceId};
pub use inward::{InwardError, StageResult, StageVerdict, StudyResult, normalize_stage_result, normalize_study_result};
pub use manifest::{
    ClosureViolation, MANIFEST_CLOSURE_BUDGET, PromptManifest, ProvenanceIndex, Slot, SlotRole, assemble_manifest,
};
pub use metrics::{
    DAYS_CAP, METRICS_DEFAULT_LIMIT, METRICS_MAX_LIMIT, MetricBloom, MetricDay, MetricDispatch, MetricsLedger,
    MetricsSeat, MetricsSummary, MetricsTimeline, RECONSTRUCTED_WINDOW, TIMELINE_SPAN_CAP, TimelineSpan, window_label,
};
pub use port::{
    BloomView, Checkpoint, ClaimHolder, ClaimOutcome, ClaimRefKind, ClaimRefState, ClaimReleaseOutcome,
    CommissionProjection, CompositionCursorView, CompositionView, Conclusion, EvidenceRef, ExecutionStatus,
    ExecutorBackend, ExecutorFaultView, HostFaultView, IntegrateOutcome, IntegrationPosition, LandOutcome,
    LandingBlock, MemberView, PendingDecisionView, ProjectedReceipt, ProjectionBackend, ReviewParkView, SourceBackend,
    SourceSnapshot, ViewDocument, WedgeCause, WorkHandle, WorkOrder,
};
pub use reduce::{
    AdjudicationError, AdmitEvidenceError, AdoptAnswerError, AggregateReviewError, AggregateReviewFault,
    AggregateVerifyError, AttemptCompletedError, BaseMismatch, BloomRecord, BloomStatus, DECISIONS_SCHEMA, Decision,
    Decisions, DecisionsSchemaError, Event, Fact, FoldConflictError, FoldedIntegration, GrantAttemptsError,
    HostFaultError, HostFaultHold, IntegrateError, LandError, LandingRejectedError, MemberExecutorFaultError,
    MemberMachineryFault, OperatorHoldError, OperatorRepairError, OrphanClaimReleaseError, Outcome, ResolveError,
    SealConflict, SealError, Snapshot, SpliceError, StageProgress, SupersedeError, VerifyFailedError,
    decode_recorded_decisions, is_active_unlanded, reduce, view_of,
};
#[cfg(not(target_arch = "wasm32"))]
pub use sign::Ed25519KeyProvider;
pub use sign::{AuthorityDoor, FakeKeyProvider, KeyProvider, SignatureEnvelope, authorization_message};
pub use spend::measure;
pub use study_report::{BloomGrade, StudyReport, grade};
pub use values::{
    ADR_SCHEMA, ADR_TRANSITION_SCHEMA, Adjudication, Adr, AdrStatus, AdrTransition, AdrValueError, AgentProfile,
    AgentSelection, ApprovalPolicy, ApprovalRule, Artifact, Attempt, BloomDraft, BloomSpec, CHECK_KEY,
    CONSTRUCT_IMPLEMENT_COMMAND, CRITICAL_KEY, CandidateRef, CatalogError, ClassifiedFinding, ClassifiedFindings,
    CommissionApprovalTier, CommissionStatementRole, CommissionStatus, CommissionValueError, CompositionFinding,
    ConfigKind, ConfigRegistry, ConfigResolveError, ConfigScopes, DependencyError, DispatchKey, Disposition, Evidence,
    EvidenceKind, ExecutionLimits, FindingClass, Forecast, Harness, JUDGMENT_TAG, LANE_WORKPIECE_HEADER,
    LandingReceipt, LongContextBand, MECHANICAL_TAG, MemberCandidate, MemberDependency, MemberSubject, Membership,
    ModelOverride, NetworkProfile, ORPHAN_CLAIM_RELEASE_WORDS, Observation, OperatorHold, OperatorRepair,
    OrphanClaimRelease, OrphanClaimReleaseCompletion, OrphanClaimReleaseRecord, OverrideError, PriceRates, PriceTable,
    Provenance, Question, REVIEW_CRITIC_COMMAND, ReasoningEffort, ResolutionClaim, ResolvedBloom, ResolvedConfigs,
    ResolvedModel, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, SealedPriceTable, SpendCeiling, SpendQuiesce,
    SpendWindow, StageBinding, StageCatalog, StageOverride, StageReceipt, Statement, StudyCall, StudyCost, StudyRecord,
    SurfacePattern, Tier, TimeoutRecord, ToolPolicy, Transformation, Unproducible, VERIFY_CHECK_COMMAND,
    VERIFY_LANE_IMAGE, VERIFY_LANE_NETWORK, VerifiedTree, VerifyFailure, VerifyFailureSet, VerifyGateSet, VerifyProof,
    VerifyReuse, Wedge, Workpiece, classify_findings, config_address, decode_config, is_model_lane,
    pin_workpiece_description, resolve_member_dependencies, split_lane_identity, surface_intersection,
};
