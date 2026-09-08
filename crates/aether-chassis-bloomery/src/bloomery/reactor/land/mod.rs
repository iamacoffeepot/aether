//! The land reactor capability (ADR-0149 migration step 3 — issue #3559).
//!
//! The last link between a resolved bloom and the mainline: it drains the
//! reducer's `aether.bloomery.land` outbox topic and issues the source-port
//! compare-and-swap that is now the landing of record, admitting the outcome back
//! through the control core. The identity/runtime split follows ADR-0122 — this
//! ZST is the addressing identity; the state-bearing logic is [`runtime`].

use std::sync::Arc;

use aether_actor::actor;
use aether_bloomery::Topic;
use aether_bloomery_github::LandingSource;

pub use runtime::{LandReactorState, LandTick};

pub struct LandReactorSetup {
    pub source: Option<Arc<dyn LandingSource>>,
    pub store_path: String,
    pub poll_interval_secs: u64,
    pub repository: Option<(String, String)>,
    pub cas_land_enabled: bool,
    /// Enqueue [`Topic::SourceReplica`] after a landing receipt is admitted.
    pub emit_source_replica: bool,
}

/// Addressing identity for the land reactor capability.
#[actor(singleton, root)]
pub struct LandReactorCapability;

impl LandReactorCapability {
    /// The outbox topics this reactor drains — its half of the producer/reactor
    /// pairing the topic tripwire checks against [`Topic::ALL`]. The land reactor
    /// is the sole drainer of [`Topic::Land`].
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::Land];
}

mod runtime;
