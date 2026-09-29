//! Barrier mail: wait until the driver's journal has processed through a sequence.

use aether_actor::HeldReply;

/// Wait until `through` is quiescent: routing has passed `through`, no routing
/// write is queued or in flight, and no request at or below `through` is
/// outstanding.
#[aether_data::kind(name = "aether.bloomery.driver.await_processed", eq, copy, no_serde)]
pub struct AwaitProcessed {
    pub through: u64,
}

/// Reply to one [`AwaitProcessed`].
#[aether_data::kind(name = "aether.bloomery.driver.processed", eq, no_serde)]
pub enum Processed {
    /// The journal head the driver observed when it answered, which
    /// includes the records it just appended. `head == through` means the
    /// graph is quiescent through that point; a larger `head` tells the
    /// caller to wait again at `head`.
    Head { head: u64 },
    /// The driver closed before the barrier was reached (ADR-0243 §1).
    Closed,
}

impl HeldReply for Processed {
    fn unanswered() -> Self {
        Self::Closed
    }
}
