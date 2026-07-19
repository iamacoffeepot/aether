//! The land-driver capability (ADR-0149 migration step 3 — issue #3559).
//!
//! The last link between a resolved bloom and the mainline: it drains the
//! reducer's `aether.bloomery.land` outbox topic and issues the source-port
//! compare-and-swap that is now the landing of record, admitting the outcome back
//! through the control core. The identity/runtime split follows ADR-0122 — this
//! ZST is the addressing identity; the state-bearing logic is [`runtime`].

use aether_actor::actor;
use aether_bloomery::Topic;

pub use runtime::{LandDriverState, LandTick};

/// Addressing identity for the land-driver capability.
#[actor(singleton)]
pub struct LandDriverCapability;

impl LandDriverCapability {
    /// The outbox topics this driver drains — its half of the producer/consumer
    /// pairing the topic tripwire checks against [`Topic::ALL`]. The land driver
    /// is the sole consumer of [`Topic::Land`].
    pub const DRAINED_TOPICS: &'static [Topic] = &[Topic::Land];
}

mod runtime;
