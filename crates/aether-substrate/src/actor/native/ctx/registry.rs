//! Registry reads and owner batches from a handler turn.
//!
//! A staged batch uses reserved owner admission and completes on a later
//! actor turn, so it rides the same ADR-0093 ledger every other deferred
//! producer does rather than taking the registry's locks mid-turn (ADR-0165).

use aether_actor::{ErasedActorRef, ReplyMode};
use aether_data::{ActorPath, KindDescriptor, KindId};

use crate::actor::native::offload::blocking::DispatchId;
use crate::mail::registry::AddressResolutionError;
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

    /// The engine's live kind vocabulary: every kind descriptor the registry
    /// holds right now, sorted by name. A component's kinds appear here the
    /// moment its load returns.
    ///
    /// Consumer: the `aether.inventory` cap's `ListKinds` handler, which
    /// projects each descriptor onto the wire.
    #[must_use]
    pub fn kind_descriptors(&self) -> Vec<KindDescriptor> {
        self.binding.kind_descriptors()
    }

    /// The origin name of one ADR-0064 tagged id, looked up in the one table
    /// its tag names: a `thr-…` id in the process thread-name registry, a
    /// `mbx-…` id in the registry's route names (so a runtime-loaded component
    /// names its lineage address), a `knd-…` id in its kind names. A miss, any
    /// other tag, or text that is not a tagged id answers `None`.
    ///
    /// The id arrives as tagged text and only a name comes back, so no mailbox
    /// position crosses this verb.
    ///
    /// Consumer: the `aether.inventory` cap's `Resolve` handler.
    #[must_use]
    pub fn tagged_id_name(&self, tagged: &str) -> Option<String> {
        self.binding.tagged_id_name(tagged)
    }

    /// The canonical path of the live actor `address` names. ADR-0166
    /// short-path expansion, canonical validation, and the liveness check are
    /// the registry's own, the same boundary the rpc server resolves an
    /// external `Call` recipient through; the answer is the path text alone,
    /// never a mailbox position.
    ///
    /// # Errors
    ///
    /// The registry's [`AddressResolutionError`] when the address is
    /// ambiguous, malformed for its root, or names no live actor.
    ///
    /// Consumer: the `aether.inventory` cap's `ResolveAddress` handler.
    pub fn canonical_path(&self, address: &ActorPath) -> Result<String, AddressResolutionError> {
        self.binding.canonical_path(address)
    }

    /// The canonical path of the actor `reference` proves, read from the
    /// published route table, for naming that actor in log and diagnostic
    /// text. The answer is path text in the ADR-0166 grammar, never a mailbox
    /// position: it reaches a position again only through the registry's
    /// address resolution. A typed holder passes `reference.erase()`.
    ///
    /// It still answers after the actor departs, because a route keeps its
    /// name through `Dropped`, and it cannot fail for a reference the
    /// registry minted: the route's name was proven against the ADR-0166
    /// grammar when the route was first published.
    ///
    /// # Panics
    ///
    /// When the route table holds no record for `reference`. The registry
    /// mints a reference only for a route that reached `Live`, and removes a
    /// route only when its birth fails (a cancelled `Starting` reservation, or
    /// a boot or spawn unwound before the actor was handed out), so this is a
    /// broken invariant, not an answer (ADR-0063).
    ///
    /// Consumers: the http server's unmonitorable route-holder warning, the
    /// component host's replacement-boot warnings, and the lifecycle cap's
    /// stuck-advance warning, which names each subscriber still owed.
    #[must_use]
    pub fn actor_path(&self, reference: ErasedActorRef) -> ActorPath {
        self.binding.actor_path(reference)
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
