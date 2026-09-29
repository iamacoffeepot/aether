//! The test-support barrier that proves the registry owner has applied
//! every batch submitted before it.

use crate::config::SettlementConfig;
use crate::mail::registry::effect::{EffectBatch, RegistryEffectError};

use super::Spawner;

impl Spawner {
    /// Block until the registry owner has applied and published every
    /// batch submitted before this call.
    ///
    /// The owner runs one FIFO queue and applies and publishes a whole
    /// drain before it completes any batch in it, so an empty barrier
    /// batch submitted after a proven ordering point completes only once
    /// everything ahead of it in that queue is applied. This proves only
    /// batches submitted *before* the call; it says nothing about a batch
    /// submitted concurrently with or after it.
    ///
    /// `gate` names the wait in the slow-log and in the panic at the
    /// settlement cap.
    ///
    /// # Panics
    /// Panics when the registry owner refuses the barrier batch or does
    /// not complete it within the settlement cap.
    pub(crate) fn await_registry_applied(&self, gate: &str) {
        let cap = SettlementConfig::from_env().to_cap();

        let Some(barrier) = self.registry.submit(EffectBatch::new(Vec::new())) else {
            return;
        };
        match barrier.wait_timeout(cap) {
            Ok(Ok(_) | Err(RegistryEffectError::OwnerClosed)) => {}
            Ok(Err(error)) => panic!("{gate}: the registry owner refused the empty barrier batch: {error}"),
            Err(error) => panic!("{gate}: the registry owner did not complete the barrier within {cap:?}: {error}"),
        }
    }
}
