//! One reactor digest's instance: driver-tracked cursor plus reactor-only health.
//!
//! An instance exists only for a digest that is ready and declares reactors.
//! Its health never reaches the program role: a poisoned digest keeps
//! answering its programs (ADR-0226 decision 2).

use aether_bloomery_kinds::Detail;

/// Reactor-only health of one digest's instance; never consulted by the program role.
#[derive(Debug)]
pub enum Health {
    /// The instance routes mail.
    Live,
    /// A fold failed; every later activation for it is rejected, and it is never reloaded.
    Poisoned(Detail),
    /// The cursor can no longer be trusted; never retried.
    Untrusted(Detail),
}

/// One reactor digest's driver-tracked cursor and health.
#[derive(Debug)]
pub struct Instance {
    /// Last seq the root has folded or evaluated.
    pub cursor: u64,
    /// Reactor-only health.
    pub health: Health,
}

impl Instance {
    /// A live instance that has folded nothing.
    pub fn new() -> Self {
        Self { cursor: 0, health: Health::Live }
    }

    /// Whether the instance routes mail.
    pub fn is_live(&self) -> bool {
        matches!(self.health, Health::Live)
    }
}
