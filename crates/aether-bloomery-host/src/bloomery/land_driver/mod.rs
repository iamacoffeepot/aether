//! The land-driver capability (ADR-0149 migration step 3 — issue #3559).
//!
//! The last link between a resolved bloom and the mainline: it drains the
//! reducer's `aether.bloomery.land` outbox topic and issues the source-port
//! compare-and-swap that is now the landing of record, admitting the outcome back
//! through the control core. The identity/runtime split follows ADR-0122 — this
//! ZST is the addressing identity; the state-bearing logic is [`runtime`].

use aether_actor::actor;

pub use runtime::{LAND_TOPIC, LandDriverState, LandTick};

/// Addressing identity for the land-driver capability.
#[actor(singleton)]
pub struct LandDriverCapability;

mod runtime;
