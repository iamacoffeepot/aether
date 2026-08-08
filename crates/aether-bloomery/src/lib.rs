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
//! - [`study_report`] — the pure forecast grade: [`grade`] folds a bloom's
//!   admitted study records into actual cost / time / retries and grades them
//!   against the sealed [`Forecast`] (ADR-0151).
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

pub mod control;
pub mod digest;
pub mod ids;
pub mod manifest;
pub mod port;
pub mod reduce;
pub mod sign;
pub mod study_report;
pub mod values;

pub use control::{
    Admit, AdmitResult, AggregateReviewPayload, CONTROL_CORE_NAMESPACE, ClaimResult, ClaimSeal, Commit, CommitResult,
    CompleteRelease, CompleteTransfer, ConfigRecord, DispatchPayload, EnumerateClaims, EnumerateClaimsResult,
    IntegratePayload, JournalRecord, LandPayload, LoadConfigs, LoadConfigsResult, MembershipMutation, ObserveMainline,
    ObserveMainlineResult, OutboxPayload, Query, QueryResult, RedispatchPayload, ReleaseSeal, ReplayJournal,
    ReplayJournalResult, ReviewPass, Topic, TransferSeal,
};
pub use digest::{ContentAddressed, Digest, digest_of};
pub use ids::{BloomId, IdempotencyKey, KeyId, Nonce, StageId, WorkpieceId};
pub use manifest::{
    ClosureViolation, MANIFEST_CLOSURE_BUDGET, PromptManifest, ProvenanceIndex, Slot, SlotRole, assemble_manifest,
};
pub use port::{
    BloomView, Checkpoint, ClaimHolder, ClaimOutcome, ClaimRefKind, ClaimRefState, Conclusion, EvidenceRef,
    ExecutionStatus, ExecutorBackend, IntegrateOutcome, IntegrationPosition, LandOutcome, LandProposal, MemberView,
    PendingDecisionView, ProjectionBackend, SourceBackend, SourceSnapshot, ViewDocument, WorkHandle, WorkOrder,
};
pub use reduce::{
    AdmitEvidenceError, AdoptAnswerError, AggregateReviewError, AttemptCompletedError, BaseMismatch, BloomRecord,
    BloomStatus, Decision, Decisions, Event, Fact, FoldedIntegration, IntegrateError, LandError, ObserveMainlineError,
    Outcome, ResolveError, SealConflict, SealError, Snapshot, StageProgress, SupersedeError, is_active_unlanded,
    reduce, view_of,
};
#[cfg(not(target_arch = "wasm32"))]
pub use sign::Ed25519KeyProvider;
pub use sign::{FakeKeyProvider, KeyProvider, SignatureEnvelope};
pub use study_report::{BloomGrade, StudyReport, grade};
pub use values::{
    AgentProfile, AgentSelection, Artifact, Attempt, BloomDraft, BloomSpec, Budget, CONSTRUCT_IMPLEMENT_COMMAND,
    CandidateRef, CatalogError, ConfigKind, ConfigRegistry, ConfigResolveError, ConfigScopes, Evidence, EvidenceKind,
    Forecast, Harness, LandingReceipt, MemberCandidate, MemberSubject, Membership, ModelOverride, NetworkProfile,
    Observation, OverrideError, Provenance, Question, REVIEW_CRITIC_COMMAND, ReasoningEffort, ResolutionClaim,
    ResolvedBloom, ResolvedConfigs, ResolvedModel, StageBinding, StageCatalog, StageOverride, StageReceipt, Statement,
    StudyCost, StudyRecord, ToolPolicy, Transformation, Unproducible, Workpiece, config_address, decode_config,
    is_model_lane,
};
