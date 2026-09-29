//! ADR-0243 §9 staged work: a blocking task that owes no reply.
//!
//! [`NativeCtx::stage_blocking`] stages a task in the turn of the request it
//! serves. Staging mints the task's [`RequestId`] from the counter outbound
//! requests use, takes the settlement hold on that turn's chain, stores the
//! task's context kind in the request-context table under the id, and arms a
//! ledger entry that owes nothing. The chain is fixed there. [`StagedTask`]
//! is what staging hands back; [`StagedTask::start`] only spawns the worker,
//! so a bounded queue can stage a request's work in that request's turn and
//! start it from whichever turn frees a slot, and the task still holds the
//! chain of the request it serves.
//!
//! When the worker finishes, the actor is woken correlated to the task's
//! request on the staging chain: the `#[handler(task)]` completion reads the
//! request from `ctx.in_reply_to()`, takes the context with
//! `ctx.take_context`, and discharges its `TaskDone` with
//! [`TaskDone::into_output`](super::blocking::TaskDone::into_output).

use std::sync::{Arc, Weak};

use aether_actor::ReplyMode;
use aether_data::{Kind, RequestId};

use super::blocking::DeferredCompletion;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;

/// Blocking work staged on the chain of the turn that staged it, not yet
/// started (ADR-0243 §9). It owes no reply.
///
/// [`Self::start`] spawns the worker. Dropping an unstarted task abandons
/// its ledger entry, releasing the chain it held, and removes the context
/// staging stored, with no panic, since nothing is owed: a queue dropped
/// with the actor's state leaves nothing behind.
#[must_use = "start the staged task; dropping it abandons the work and releases the chain it holds"]
pub struct StagedTask<O> {
    request: RequestId,
    /// The ledger entry the worker fills. `None` once [`Self::start`] moved
    /// it onto the worker.
    completion: Option<DeferredCompletion<O>>,
    /// Removes the context staging stored under `request`, typed by its
    /// kind, when the task is dropped unstarted. `None` for a task staged
    /// with no context.
    discard_context: Option<DiscardContext>,
}

/// The typed take an unstarted task's drop runs to remove its stored
/// context, holding the binding weakly as the task's completion does.
struct DiscardContext {
    binding: Weak<NativeBinding>,
    discard: fn(&Arc<NativeBinding>, RequestId),
}

impl<O: Send + 'static> StagedTask<O> {
    pub(crate) fn new(request: RequestId, completion: DeferredCompletion<O>) -> Self {
        Self { request, completion: Some(completion), discard_context: None }
    }

    /// Record that staging stored a context of kind `C` under this task's
    /// request, so an unstarted drop removes it.
    pub(crate) fn with_context<C: Kind>(mut self, binding: &Arc<NativeBinding>) -> Self {
        self.discard_context = Some(DiscardContext { binding: Arc::downgrade(binding), discard: discard::<C> });
        self
    }

    /// The request id the task's completion is correlated to: what
    /// `ctx.in_reply_to()` returns in its `#[handler(task)]` completion.
    #[must_use]
    pub fn request(&self) -> RequestId {
        self.request
    }

    /// Spawn the worker that runs `work` and wakes the actor with its output.
    /// The chain the task holds was fixed when it was staged, so the turn
    /// `ctx` dispatches never affects it. Returns the task's request id.
    ///
    /// A panic in `work` is fatal (ADR-0063), as for every sanctioned
    /// offload worker.
    ///
    /// # Panics
    /// Never in practice: only `start` takes the task's completion, and it
    /// consumes the task.
    pub fn start<A, M, F>(mut self, ctx: &NativeCtx<'_, A, M>, work: F) -> RequestId
    where
        M: ReplyMode,
        F: FnOnce() -> O + Send + 'static,
    {
        self.discard_context = None;
        let completion = self.completion.take().expect("an unstarted staged task holds its completion");
        ctx.spawn_blocking_worker(completion, work);
        self.request
    }
}

impl<O> Drop for StagedTask<O> {
    /// An unstarted task removes its stored context, then abandons its entry,
    /// releasing its hold. A started one owns nothing any more.
    fn drop(&mut self) {
        if let Some(DiscardContext { binding, discard }) = self.discard_context.take()
            && let Some(binding) = binding.upgrade()
        {
            discard(&binding, self.request);
        }
        drop(self.completion.take());
    }
}

/// Take and drop the context of kind `C` stored under `request`. A `Held`
/// it carries comes back live and fails fast as it drops, as any dropped
/// context's debt does (ADR-0243 §4); at actor close the ledger has already
/// answered it, so the take finds nothing to claim.
fn discard<C: Kind>(binding: &Arc<NativeBinding>, request: RequestId) {
    drop(binding.take_request_context::<C>(request));
}
