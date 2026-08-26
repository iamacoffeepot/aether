//! The typed port trait shapes (ADR-0149 §The boundary).
//!
//! Bloomery's boundary is a set of typed ports, each owned by a native
//! capability so the wasm application never touches keys, databases, tokens,
//! or shells. The trait *shapes* live here — not host-side — so adapters
//! depend inward on this crate, cycle-free: [#3459]'s GitHub adapter
//! implements [`SourceBackend`] / [`ProjectionBackend`] and the host
//! ([#3458]) mounts them behind `Arc<dyn …>`. If the traits lived host-side,
//! an adapter implementing a host trait while the host statically links the
//! adapter would be a package-level dependency cycle. Adapters depend inward
//! on this crate, never the reverse.
//!
//! The `source` / `projection` / `executor` ports ship here — each with a
//! first consumer in the crate DAG. The executor's is migration step 2's
//! Actions dispatch backend ([#3500]). The `store` / `signing` port shapes
//! arrive with their first consumers, not speculatively (ADR-0149 §The
//! boundary).
//!
//! These are pure trait definitions over the value vocabulary — the
//! implementations do the I/O; the contracts are protocol semantics. No I/O
//! occurs in this crate.
//!
//! [#3458]: https://github.com/iamacoffeepot/aether/issues/3458
//! [#3459]: https://github.com/iamacoffeepot/aether/issues/3459
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500

mod executor;
mod projection;
mod row;
mod source;

pub use executor::{
    BackendId, Conclusion, EvidenceRef, ExecutionStatus, ExecutorBackend, LaneObservation, ObservedLaneWrites,
    WorkHandle, WorkOrder,
};
pub use projection::{
    AwaitingSurfaceView, BaseAlertView, BloomView, CommissionProjection, CompositionCursorView, CompositionView,
    ExecutorFaultView, HostFaultView, LandingBlock, LeaseEvictionView, LeaseView, MAX_TITLE_CHARS, MemberView,
    MemberWhy, NarrowedCompositionView, PendingDecisionView, ProjectedReceipt, ProjectionBackend, ReviewParkView,
    TransitionWhy, ViewDocument, WedgeCause, WhyDocument, WhyState, WithdrawnView, intent_title,
};
pub use row::{POSITIONAL_ROW_SCHEMA, RowSchemaError, decode_row, encode_row};
pub use source::{
    Checkpoint, ClaimHolder, ClaimOutcome, ClaimRefKind, ClaimRefState, ClaimReleaseOutcome, IntegrateOutcome,
    IntegrationPosition, LandOutcome, SourceBackend, SourceSnapshot,
};
