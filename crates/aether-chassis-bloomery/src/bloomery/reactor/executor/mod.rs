//! The `aether.bloomery.executor` dispatch reactor capability (ADR-0149 migration
//! step 2 — issue #3505).
//!
//! The reducer decides to dispatch a per-member attempt and enqueues a
//! [`DispatchPayload`](aether_bloomery::DispatchPayload) on the store's
//! [`Topic::Dispatch`] outbox topic; nothing
//! drained it until this reactor. It is the executor-dispatch reactor the
//! reducer's producer side (#3497) and the `MirrorReactorCapability` doc both
//! reserved: a poll-driven drain that submits each dispatch through the
//! [`ExecutorShell`] and records its intake
//! context, then pulls matched attempt
//! results back and admits them to the `aether.bloomery.control` actor — closing
//! the line into a moving loop.
//!
//! Identity/runtime split (ADR-0122), mirroring the store and the mirror reactor:
//! the [`ExecutorReactorCapability`] ZST identity lives here; the state-bearing
//! `#[runtime] impl NativeActor` — the poll timer, the drain → submit → pull →
//! admit state machine, and the config-gating — lives in `runtime.rs`.

use std::sync::Arc;

use crate::bloomery::ExecutorShell;
// The scripted-lane seam's mail and reply kinds (#4711). Imported here, beside
// the ZST, because `#[actor]` re-emits every handler's kinds in *this* module —
// the same reason `DispatchTick` is re-exported below rather than left in
// `runtime`. Gated exactly as the handler is, so a production build imports
// nothing.
#[cfg(any(test, feature = "testing"))]
use crate::bloomery::testing::{ScriptedEvidence, ScriptedEvidenceResult};
use aether_actor::actor;
use aether_bloomery::AdmitResult;
use aether_bloomery::SharedCorrespondence;
use aether_bloomery::Topic;

// `pub` rather than `pub(crate)` because this module is itself private: the
// crate-only restriction is applied once, where the chain reaches the public
// `bloomery` module, and repeating it here is the redundancy clippy flags.
pub use runtime::candidate_push_at;
pub use runtime::{CandidatePush, DispatchTick, ExecutorReactorState};

pub struct ExecutorReactorSetup {
    pub executor: Option<ExecutorShell>,
    pub correspondence: Option<SharedCorrespondence>,
    pub store_path: String,
    pub artifacts_root: Option<String>,
    pub poll_interval_secs: u64,
    pub stale_warn_after_secs: u64,
    /// How long a local model lane may stay silent before this host cancels it
    /// (ADR-0195 §8). Already validated nonzero at chassis boot.
    pub heartbeat_silence_secs: u64,
    pub repository: Option<(String, String)>,
    pub disabled_missing: Vec<&'static str>,
    /// The candidate-ref push seam (ADR-0152); chosen at boot by
    /// `default_candidate_push`, which is crate-private and so is named here
    /// rather than linked.
    pub pusher: Arc<dyn CandidatePush>,
}

/// Addressing identity for the executor dispatch reactor capability.
#[actor(singleton, root)]
pub struct ExecutorReactorCapability;

impl ExecutorReactorCapability {
    /// The outbox topics this reactor drains — its half of the producer/reactor
    /// pairing the topic tripwire checks against [`Topic::ALL`]. The executor
    /// reactor is the sole drainer of the per-member [`Topic::Dispatch`], the
    /// whole-bloom [`Topic::AggregateReview`] (ADR-0153), and the
    /// [`Topic::Redispatch`] replay of an answered parked question (ADR-0151,
    /// #3664) — all three submit through the same executor shell and ride the
    /// same intake cycle.
    pub const DRAINED_TOPICS: &'static [Topic] =
        &[Topic::Dispatch, Topic::AggregateReview, Topic::AggregateVerify, Topic::Redispatch];
}

mod runtime;
