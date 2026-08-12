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

use crate::bloomery::ExecutorShell;
use aether_actor::actor;
use aether_bloomery::SharedCorrespondence;
use aether_bloomery::Topic;

pub use runtime::{DispatchTick, ExecutorReactorState};

pub struct ExecutorReactorSetup {
    pub executor: Option<ExecutorShell>,
    pub correspondence: Option<SharedCorrespondence>,
    pub store_path: String,
    pub artifacts_root: Option<String>,
    pub poll_interval_secs: u64,
    pub stale_warn_after_secs: u64,
    pub repository: Option<(String, String)>,
    pub disabled_missing: Vec<&'static str>,
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
