//! Barrier mail: wait until the driver's journal has processed through a sequence.

/// Wait until the driver's journal head is at or past `through`.
#[aether_data::kind(name = "aether.bloomery.driver.await_processed", eq, copy, no_serde)]
pub struct AwaitProcessed {
    pub through: u64,
}

/// Reply to one [`AwaitProcessed`].
///
/// `head` is the journal head the driver observed when it answered, which
/// includes the records it just appended. `head == through` means the
/// graph is quiescent through that point; a larger `head` tells the caller
/// to wait again at `head`.
#[aether_data::kind(name = "aether.bloomery.driver.processed", eq, no_serde)]
pub struct Processed {
    pub head: u64,
}
