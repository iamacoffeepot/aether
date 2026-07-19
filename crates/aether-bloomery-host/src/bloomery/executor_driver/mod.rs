//! The `aether.bloomery.executor` dispatch-driver capability (ADR-0149 migration
//! step 2 — issue #3505).
//!
//! The reducer decides to dispatch a per-member attempt and enqueues a
//! [`DispatchPayload`](aether_bloomery::DispatchPayload) on the store's
//! [`Topic::DISPATCH`](aether_bloomery::Topic::DISPATCH) outbox topic; nothing
//! drained it until this
//! capability. It is the executor-dispatch consumer the reducer's producer side
//! (#3497) and the `MirrorDriverCapability` doc both reserved: a poll-driven drain
//! that submits each dispatch through the [`ExecutorShell`] and records its intake
//! context, then pulls matched attempt results back and admits them to the
//! `aether.bloomery.control` actor — closing the line into a moving loop.
//!
//! Identity/runtime split (ADR-0122), mirroring the store and the mirror driver:
//! the [`ExecutorDriverCapability`] ZST identity lives here; the state-bearing
//! `#[runtime] impl NativeActor` — the poll timer, the drain → submit → pull →
//! admit state machine, and the config-gating — lives in `runtime.rs`.

use aether_actor::actor;
use aether_bloomery::Topic;

pub use runtime::{DispatchTick, ExecutorDriverState};

/// Addressing identity for the executor dispatch-driver capability.
#[actor(singleton)]
pub struct ExecutorDriverCapability;

impl ExecutorDriverCapability {
    /// The outbox topics this driver drains — its half of the producer/consumer
    /// pairing the topic tripwire checks against [`Topic::ALL`]. The executor
    /// driver is the sole consumer of both the per-member [`Topic::DISPATCH`]
    /// and the whole-bloom [`Topic::AGGREGATE_REVIEW`] (ADR-0153).
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::DISPATCH, Topic::AGGREGATE_REVIEW];
}

mod runtime;
