//! The propose reactor capability (ADR-0205).
//!
//! Drains the reducer's `topic:proposal` outbox topic, seals the proposal's
//! configuration bytes, pushes the candidate ref under the resolved bloom id,
//! and admits a memberless [`aether_bloomery::Fact::Seal`] through the control
//! core. The identity/runtime split follows ADR-0122 — this ZST is the
//! addressing identity; the state-bearing logic is [`runtime`].

use std::sync::Arc;

use aether_actor::actor;
use aether_bloomery::{SharedCorrespondence, Topic};

use crate::bloomery::CandidatePush;

pub use runtime::{ProposeReactorState, ProposeTick};

/// Composer-supplied parts for the propose reactor.
pub struct ProposeReactorSetup {
    /// The correspondence the submission route already recorded the candidate
    /// against. `None` mounts the reactor disabled.
    pub correspondence: Option<SharedCorrespondence>,
    /// The candidate-ref push seam. `None` mounts the reactor disabled.
    pub pusher: Option<Arc<dyn CandidatePush>>,
    /// The store the outbox topic is drained from.
    pub store_path: String,
    /// How often to wake and drain.
    pub poll_interval_secs: u64,
    /// Whether to force-push the candidate ref. Fixture boots have no
    /// publish remote — correspondence is the checkout — so this is `false`
    /// there. Production always publishes.
    pub publish_candidate: bool,
}

/// Addressing identity for the propose reactor capability.
#[actor(singleton, root)]
pub struct ProposeReactorCapability;

impl ProposeReactorCapability {
    /// The outbox topics this reactor drains — its half of the producer/reactor
    /// pairing the topic tripwire checks against [`Topic::ALL`]. It is the sole
    /// drainer of [`Topic::Proposal`].
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::Proposal];
}

mod runtime;
