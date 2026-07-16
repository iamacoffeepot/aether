//! aether-bloomery: the canonical value vocabulary and pure control core
//! of Bloomery (ADR-0149).
//!
//! This is the second slice of ADR-0149 — an rlib now, the cdylib actors
//! later — and it provides the two things the whole control plane rests
//! on: a canonical, immutable value vocabulary that everything durable is
//! expressed in, and the one pure function that owns every state
//! transition.
//!
//! # Wasm-safe by construction
//!
//! The crate is `#![no_std]` + `alloc`, matching `aether-data`'s dual-target
//! shape, so the later cdylib-actors slice compiles the same types to wasm
//! and the native host ([#3458]) links them without a fork. Native `std`
//! appears only under `#[cfg(test)]` — the integration tests in `tests/`
//! are ordinary std crates.
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

// `std`, not `#![no_std]` (issue #3497): the crate emits a `cdylib` control-core
// wasm component (ADR-0149 §Packaging, the `aether-kit` precedent), and a
// `cdylib` target must build for the native host — where a `#![no_std]` crate has
// no global allocator. `std` compiles to `wasm32` like every component here, and
// no consumer requires `no_std`. The pure core still leans on `alloc` paths, so
// the crate is kept `alloc`-explicit for a clean wasm-safe surface.
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

#[cfg(feature = "runtime")]
pub use control::ControlCore;
pub use control::{
    Admit, AdmitResult, ClaimResult, ClaimSeal, Commit, CommitResult, JournalRecord, MembershipMutation, OutboxPayload,
    Query, QueryResult, ReleaseSeal, ReplayJournal, ReplayJournalResult, TransferSeal,
};
pub use digest::{ContentAddressed, Digest, digest_of};
pub use ids::{BloomId, IdempotencyKey, KeyId, Nonce, StageId, WorkpieceId};
pub use manifest::{
    ClosureViolation, MANIFEST_CLOSURE_BUDGET, PromptManifest, ProvenanceIndex, Slot, SlotRole, assemble_manifest,
};
pub use port::{
    BloomView, Checkpoint, ClaimOutcome, ClaimRefKind, Conclusion, EvidenceRef, ExecutionStatus, ExecutorBackend,
    IntegrateOutcome, LandOutcome, MemberView, PendingDecisionView, ProjectionBackend, SourceBackend, SourceSnapshot,
    ViewDocument, WorkHandle, WorkOrder,
};
pub use reduce::{
    AdmitEvidenceError, AdoptAnswerError, BaseMismatch, BloomRecord, BloomStatus, Decision, Decisions, Event, Fact,
    IntegrateError, LandError, Outcome, ResolveError, SealConflict, SealError, Snapshot, SupersedeError,
    is_active_unlanded, reduce, view_of,
};
pub use sign::{FakeKeyProvider, KeyProvider, SignatureEnvelope};
pub use study_report::{BloomGrade, StudyReport, grade};
pub use values::{
    AgentProfile, Artifact, Attempt, BloomDraft, BloomSpec, Budget, Evidence, EvidenceKind, Forecast, LandingReceipt,
    Membership, ModelOverride, NetworkProfile, Observation, Provenance, Question, ReasoningEffort, ResolutionClaim,
    ResolvedBloom, ResolvedModel, ScopeRevision, StageBinding, StageCatalog, StageReceipt, Statement, StudyCost,
    StudyRecord, ToolPolicy, Transformation, Workpiece,
};
