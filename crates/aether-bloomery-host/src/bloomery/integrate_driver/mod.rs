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

pub use runtime::{INTEGRATE_TOPIC, IntegrateDriverState, IntegrateTick};

/// Addressing identity for the integrate-driver capability.
#[actor(singleton)]
pub struct IntegrateDriverCapability;

mod runtime;
