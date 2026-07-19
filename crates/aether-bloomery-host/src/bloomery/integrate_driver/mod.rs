//! The integrate-driver capability (ADR-0152 §Resolution drives integration —
//! issue #3650).
//!
//! The bridge between recorded resolutions and the landable head: it drains the
//! reducer's `aether.bloomery.integrate` outbox topic, folds every member's
//! claimed candidate tree onto the bloom's integration branch through the
//! source port's CAS-guarded `integrate`, and admits the resulting
//! `Fact::Resolve` back through the control core — whose `DispatchLand` the
//! existing land driver then consumes. The identity/runtime split follows
//! ADR-0122 — this ZST is the addressing identity; the state-bearing logic is
//! [`runtime`].

use aether_actor::actor;
use aether_bloomery::Topic;

pub use runtime::{IntegrateDriverState, IntegrateTick};

/// Addressing identity for the integrate-driver capability.
#[actor(singleton)]
pub struct IntegrateDriverCapability;

impl IntegrateDriverCapability {
    /// The outbox topics this driver drains — its half of the producer/consumer
    /// pairing the topic tripwire checks against [`Topic::ALL`]. The integrate
    /// driver is the sole consumer of [`Topic::INTEGRATE`].
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::INTEGRATE];
}

mod runtime;
