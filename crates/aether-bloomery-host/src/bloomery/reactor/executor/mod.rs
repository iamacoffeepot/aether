//! The `aether.bloomery.executor` dispatch reactor capability (ADR-0149 migration
//! step 2 — issue #3505).
//!
//! The reducer decides to dispatch a per-member attempt and enqueues a
//! [`DispatchPayload`](aether_bloomery::DispatchPayload) on the store's
//! [`Topic::Dispatch`](aether_bloomery::Topic::Dispatch) outbox topic; nothing
//! drained it until this reactor. It is the executor-dispatch reactor the
//! reducer's producer side (#3497) and the `MirrorReactorCapability` doc both
//! reserved: a poll-driven drain that submits each dispatch through the
//! [`ExecutorShell`] and records its intake context, then pulls matched attempt
//! results back and admits them to the `aether.bloomery.control` actor — closing
//! the line into a moving loop.
//!
//! Identity/runtime split (ADR-0122), mirroring the store and the mirror reactor:
//! the [`ExecutorReactorCapability`] ZST identity lives here; the state-bearing
//! `#[runtime] impl NativeActor` — the poll timer, the drain → submit → pull →
//! admit state machine, and the config-gating — lives in `runtime.rs`.

use aether_actor::actor;
use aether_bloomery::Topic;

pub use runtime::{DispatchTick, ExecutorReactorState};

/// Addressing identity for the executor dispatch reactor capability.
#[actor(singleton)]
pub struct ExecutorReactorCapability;

impl ExecutorReactorCapability {
    /// The outbox topics this reactor drains — its half of the producer/reactor
    /// pairing the topic tripwire checks against [`Topic::ALL`]. The executor
    /// reactor is the sole drainer of both the per-member [`Topic::Dispatch`]
    /// and the whole-bloom [`Topic::AggregateReview`] (ADR-0153).
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::Dispatch, Topic::AggregateReview];
}

mod runtime;
