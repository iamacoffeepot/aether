//! Registry reads and owner batches from a handler turn.
//!
//! A staged batch uses reserved owner admission and completes on a later
//! actor turn, so it rides the same ADR-0093 ledger every other deferred
//! producer does rather than taking the registry's locks mid-turn (ADR-0165).

use aether_actor::ReplyMode;
use aether_data::KindId;

use crate::actor::native::offload::blocking::DispatchId;
use crate::mail::registry::effect::{RegistryBatch, RegistryBatchResult};

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// A kind's display label for log and diagnostic text: its registered
    /// name, or the tagged `knd-…` id's text when the kind is not registered
    /// (a component-defined kind the registry has not seen).
    ///
    /// Consumers: the wasm trampoline's fallback, which labels an inbound kind
    /// in its no-wasm warning and its trap abort, and the rpc server's reply
    /// fallback, which labels an unmatched reply in its debug line.
    #[must_use]
    pub fn kind_label(&self, kind: KindId) -> String {
        self.binding.kind_label(kind)
    }

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
