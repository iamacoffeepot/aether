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
//!   [`LandingReceipt`]), [`Evidence`], [`StageBinding`] / [`StageCatalog`],
//!   [`Attempt`], [`Transformation`].
//! - [`sign`] — the [`KeyProvider`] trait and its fake always-valid
//!   implementation. ADR-0149 ships the signature *shapes*; key custody is
//!   a later arc.
//! - [`manifest`] — [`PromptManifest`] assembly, fail-closed: an
//!   instruction-capable slot that does not trace to a signed statement or
//!   a versioned policy artifact rejects the attempt before dispatch.
//! - [`mod@reduce`] — the pure control core: [`reduce`](reduce::reduce) owns
//!   every state transition, with no I/O, no engine boot, no GitHub types.
//! - [`port`] — the [`SourceBackend`] / [`ProjectionBackend`] trait shapes
//!   adapters implement and the host mounts. Kept here so adapters depend
//!   inward on this crate, cycle-free (ADR-0149 §The boundary).
//!
//! [#3458]: https://github.com/iamacoffeepot/aether/issues/3458

#![no_std]

extern crate alloc;

pub mod digest;
pub mod ids;
pub mod manifest;
pub mod port;
pub mod reduce;
pub mod sign;
pub mod values;

pub use digest::{ContentAddressed, Digest, digest_of};
pub use ids::{BloomId, IdempotencyKey, KeyId, StageId, WorkpieceId};
pub use manifest::{
    ClosureViolation, MANIFEST_CLOSURE_BUDGET, PromptManifest, ProvenanceIndex, Slot, SlotRole, assemble_manifest,
};
pub use port::{
    BloomView, Checkpoint, IntegrateOutcome, LandOutcome, MemberView, ProjectionBackend, SourceBackend, SourceSnapshot,
    ViewDocument,
};
pub use reduce::{
    BaseMismatch, BloomRecord, BloomStatus, Decision, Decisions, Event, Fact, IntegrateError, LandError, Outcome,
    ResolveError, SealConflict, SealError, Snapshot, SupersedeError, reduce, view_of,
};
pub use sign::{FakeKeyProvider, KeyProvider, SignatureEnvelope};
pub use values::{
    Artifact, Attempt, BloomDraft, BloomSpec, Budget, Evidence, EvidenceKind, Forecast, LandingReceipt, Membership,
    Observation, Provenance, ResolutionClaim, ResolvedBloom, StageBinding, StageCatalog, StageReceipt, Statement,
    Transformation, Workpiece,
};
