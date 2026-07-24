//! The integrate reactor capability (ADR-0152 §Resolution drives integration —
//! issue #3650).
//!
//! The bridge between recorded resolutions and the landable head: it drains the
//! reducer's `aether.bloomery.integrate` outbox topic, folds every member's
//! claimed candidate tree onto the bloom's integration branch through the
//! source port's CAS-guarded `integrate`, and admits the resulting
//! `Fact::Resolve` back through the control core — whose `DispatchLand` the
//! land reactor then drains. The identity/runtime split follows
//! ADR-0122 — this ZST is the addressing identity; the state-bearing logic is
//! [`runtime`].

use aether_actor::actor;
use aether_bloomery::Topic;

pub use runtime::{IntegrateReactorState, IntegrateTick};

/// Addressing identity for the integrate reactor capability.
#[actor(singleton, root)]
pub struct IntegrateReactorCapability;

impl IntegrateReactorCapability {
    /// The outbox topics this reactor drains — its half of the producer/reactor
    /// pairing the topic tripwire checks against [`Topic::ALL`]. The integrate
    /// reactor is the sole drainer of [`Topic::Integrate`].
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::Integrate];
}

mod runtime;
