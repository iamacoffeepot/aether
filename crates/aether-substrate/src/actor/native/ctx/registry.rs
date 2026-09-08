//! Staging a typed registry-owner batch from a handler turn.
//!
//! The batch uses reserved owner admission and completes on a later actor
//! turn, so it rides the same ADR-0093 ledger every other deferred producer
//! does rather than taking the registry's locks mid-turn (ADR-0165).

use aether_actor::ReplyMode;

use crate::actor::native::offload::blocking::DispatchId;
use crate::mail::registry::effect::{RegistryBatch, RegistryBatchResult};

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, M, A> {
    /// Stage a typed registry-owner batch from the current handler. The batch
    /// uses reserved owner admission and completes on a later actor turn.
    pub fn stage_registry_batch<C>(&mut self, batch: RegistryBatch, context: C) -> DispatchId
    where
        C: Send + 'static,
    {
        let completion = self.arm_deferred_completion::<RegistryBatchResult, _>(context);
        let id = completion.dispatch_id();
        self.binding.stage_owner_batch(batch, completion);
        id
    }
}
