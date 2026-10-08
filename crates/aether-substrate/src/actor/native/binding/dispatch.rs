//! The ADR-0093 hold-until-resolve in-flight ledger — the `&self`
//! interior-mutability bridge between a handler's dispatch primitive and the
//! per-actor table that parks its hold.

use std::any::Any;
use std::sync::{Arc, Weak};

use super::NativeBinding;
use super::offload::blocking::{
    CompletionWake, DeferredCompletion, DispatchId, FillOutcome, OwedAtClose, TaskCompletionWake, TaskDone,
};
use super::offload::held::AnswerUnanswered;
use crate::mail::{KindId, Mail, Source, SourceAddr};
use crate::runtime::trace::SettlementHold;
use aether_data::{Kind, RequestId, wire};

/// ADR-0093 hold-until-resolve dispatch: the `&self`-interior-mutability
/// bridge between [`super::ctx::NativeCtx`](crate::actor::native::ctx::NativeCtx)'s dispatch primitive and the
/// per-actor `InflightLedger` (crate-internal). Each method
/// takes the ledger lock for one operation — mint+insert at dispatch,
/// fill-output from the worker, take at completion — except the reply
/// tail's parked-context check, which skips it while nothing is parked. The
/// ledger is a `Mutex` because it has writers off the actor's dispatch
/// thread: offload workers, child activations and the registry owner
/// completing deferred work, a dropped `Held`, and close and teardown.
impl NativeBinding {
    /// Insert a freshly-minted in-flight dispatch entry and return its
    /// [`DispatchId`]. Called on the actor thread at dispatch time, after
    /// the hold is acquired and before the worker spawns. `hold` is `None` when the dispatching
    /// context carried no chain to hold (ADR-0168 §2).
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_insert(
        &self,
        hold: Option<SettlementHold>,
        reply_to: Source,
        context: Box<dyn Any + Send>,
    ) -> DispatchId {
        self.inflight.lock().dispatch_insert(hold, reply_to, context)
    }

    /// Arm a typed deferred completion in the ordinary ADR-0093 ledger.
    /// The returned move-only capability retains this binding only weakly.
    pub(crate) fn dispatch_arm<O, C>(
        self: &Arc<Self>,
        hold: Option<SettlementHold>,
        reply_to: Source,
        context: C,
    ) -> DeferredCompletion<O>
    where
        C: Send + 'static,
    {
        let dispatch_id = self.dispatch_insert(hold, reply_to, Box::new(context));
        DeferredCompletion::new(Arc::downgrade(self), dispatch_id)
    }

    /// Arm a staged task's ledger entry (ADR-0243 §9): it keeps `hold`, the
    /// staging turn's chain, owes no reply, and completes correlated to
    /// `request`. The returned move-only capability retains this binding
    /// only weakly.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_stage<O>(
        self: &Arc<Self>,
        hold: Option<SettlementHold>,
        request: RequestId,
    ) -> DeferredCompletion<O> {
        let dispatch_id = self.inflight.lock().dispatch_insert_task(hold, request);
        DeferredCompletion::new(Arc::downgrade(self), dispatch_id)
    }

    /// Shared deferred-completion tail. Fill the named dispatch's output
    /// slot under the ledger mutex, drop the lock, then push exactly one
    /// [`TaskCompletionWake`] for the winning fill.
    ///
    /// A worker entry's wake is the unchained loopback [`Self::wake_self`]
    /// pushes. A staged task's wake is correlated to its request, so its
    /// completion reads the request from `in_reply_to` and takes the context
    /// stored under it, and carries the root its hold keeps open, so the
    /// completion's sends inherit the staging chain (ADR-0243 §9). It has no
    /// mail id: settlement counts no in-flight mail for it, and `sender()`
    /// names no one. The entry keeps the hold through the fill, and the
    /// completion's ctx takes it and releases it when that handler ends, so
    /// the chain stays open from staging until every send the completion
    /// makes is counted.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_complete<O>(&self, id: DispatchId, output: O)
    where
        O: Send + 'static,
    {
        let filled = self.inflight.lock().dispatch_fill_output(id, Box::new(output));
        let FillOutcome::Filled(wake) = filled else {
            return;
        };

        let bytes = TaskCompletionWake { dispatch_id: id.0 }.encode_into_bytes();
        match wake {
            CompletionWake::Unchained => self.wake_self(TaskCompletionWake::ID, bytes),
            CompletionWake::Task { request, root } => self.mailer.push(
                Mail::new(self.self_mailbox(), TaskCompletionWake::ID, bytes, 1)
                    .with_reply_to(Source::with_correlation(SourceAddr::None, request.0))
                    .with_lineage(None, root, None),
            ),
        }
    }

    /// Push one already-encoded mail to this actor's own mailbox as an
    /// unchained loopback wake: no parent, no root, the default reply
    /// target. Only the deferred-completion tail wakes through it: that mail
    /// is the engine's own, so its completion reads no sender. A wake a
    /// capability's helper thread sends goes through [`Self::wake_as_self`].
    pub(crate) fn wake_self(&self, kind: KindId, bytes: Vec<u8>) {
        self.mailer.push(Mail::new(self.self_mailbox(), kind, bytes, 1));
    }

    /// Push one already-encoded mail to this actor's own mailbox as an
    /// unchained loopback wake that names the actor as its sender. A
    /// [`SelfWake`](super::offload::self_wake::SelfWake), the handle an
    /// off-thread helper holds in place of a position plus a mailer, wakes
    /// through it: the helper acts for its actor, so the actor is the sender.
    /// The stamp carries no correlation, so it is no reply and settlement
    /// counts the wake as it counts any unchained push.
    pub(crate) fn wake_as_self(&self, kind: KindId, bytes: Vec<u8>) {
        let sender = Source::to(SourceAddr::Component(self.self_mailbox()));
        self.mailer.push(Mail::new(self.self_mailbox(), kind, bytes, 1).with_reply_to(sender));
    }

    /// Remove the named dispatch entry and rebuild its [`TaskDone`]. Called
    /// on the actor thread when the completion-wake mail lands.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_take<O: 'static, C: 'static>(&self, id: DispatchId) -> Option<TaskDone<O, C>> {
        self.inflight.lock().dispatch_take(id)
    }

    /// Remove the named dispatch entry and hand back its hold without any
    /// downcast — the release path for a worker that never ran. The
    /// spawn-error branch of the `dispatch_blocking*` worker spawn and an
    /// unstarted staged task's drop call this and drop the returned hold,
    /// after the ledger lock is released, so the chain settles.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_abandon(&self, id: DispatchId) -> Option<SettlementHold> {
        self.inflight.lock().dispatch_abandon(id)
    }

    /// Non-consuming peek-then-take of the named dispatch entry: probe its
    /// boxed output + context against `O` / `C` and only remove + rebuild
    /// the [`TaskDone`] on a match,
    /// leaving the entry intact on a mismatch. The `#[handler(task)]`
    /// dispatch chain calls this to route a completion to the right
    /// output-typed handler without a wrong-type probe consuming the entry.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_try_take<O: 'static, C: 'static>(&self, id: DispatchId) -> Option<TaskDone<O, C>> {
        self.inflight.lock().dispatch_try_take(id)
    }

    /// Arm a ledger entry no worker answers (ADR-0243 §1), with the `answer`
    /// actor close sends for it, and return its [`DispatchId`] with the weak
    /// link its [`Held`](super::offload::held::Held) ticket keeps back to
    /// this ledger.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_hold(
        self: &Arc<Self>,
        hold: Option<SettlementHold>,
        reply_to: Source,
        answer: AnswerUnanswered,
    ) -> (DispatchId, Weak<Self>) {
        let id = self.inflight.lock().dispatch_insert_held(hold, reply_to, answer);
        (id, Arc::downgrade(self))
    }

    /// Remove the named held entry and hand back its parked
    /// `(Option<SettlementHold>, Source)`. `None` for an unknown id or an
    /// entry a worker answers.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_claim_held(&self, id: DispatchId) -> Option<(Option<SettlementHold>, Source)> {
        self.inflight.lock().dispatch_claim_held(id)
    }

    /// Hand the held entry `id` to a worker (ADR-0243 §3): the entry keeps
    /// the hold and reply target its `Held` ticket named and parks `context`
    /// for the completion. The returned capability fills that same entry, so
    /// the worker's completion answers the obligation the ticket's receipt
    /// declared.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063 — and when `id` names no held entry.
    pub(crate) fn dispatch_attach_worker<O>(
        self: &Arc<Self>,
        id: DispatchId,
        context: Box<dyn Any + Send>,
    ) -> DeferredCompletion<O> {
        self.inflight.lock().dispatch_attach_worker(id, context);
        DeferredCompletion::new(Arc::downgrade(self), id)
    }

    /// Answer every held and parked entry still in the ledger with its reply
    /// kind's `unanswered` value, and release every staged task with no
    /// reply, because the actor that owes them is closing while the engine
    /// keeps running (ADR-0243 §1, §9): `ctx.shutdown()`, a cancelled
    /// activation. Each answer is sent before its hold releases, so `Sent` precedes
    /// `Release` as for `Held::answer`. The entries are collected under the
    /// ledger lock and answered after it is released, because a reply may
    /// route synchronously into an inbox handler. The close paths call it
    /// before the actor's state drops, so a `Held` or an unstarted staged
    /// task parked in that state then finds its entry gone and drops
    /// silently.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn answer_held_for_actor_close(&self) {
        let (owed, released) = self.inflight.close_for_actor();
        for OwedAtClose { hold, reply_to, answer } in owed {
            answer(self, reply_to, hold.as_ref().map(SettlementHold::root));
            drop(hold);
        }
        drop(released);
    }

    /// Release every held and parked entry and every staged task still in
    /// the ledger with no reply and no panic, because the actor is closing
    /// as part of engine teardown (ADR-0243 §1): every requester is closing
    /// with the engine, so an answer would reach only closing actors. The
    /// teardown close paths call it before the actor's state drops, so a
    /// `Held` or an unstarted staged task in that state then finds its entry
    /// gone and drops silently. The holds release after the ledger lock is
    /// released.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn settle_held_for_engine_teardown(&self) {
        let (owed, released) = self.inflight.close_for_actor();
        drop(owed);
        drop(released);
    }

    /// Park the held entry `id` in the request context stored under
    /// `request` (ADR-0243 §4). Takes the ledger lock for this one
    /// operation; its caller holds `request_contexts`, the one nesting the
    /// lock order allows.
    ///
    /// # Errors
    /// [`wire::Error::HeldUnclaimed`] when `id` names no held entry.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_park(
        &self,
        id: DispatchId,
        request: RequestId,
        reply: KindId,
        context_name: &'static str,
    ) -> Result<(), wire::Error> {
        self.inflight.park(id, request, reply, context_name)
    }

    /// Claim the parked entry `id` back to held for a decode of the context
    /// stored under `request`. Takes the ledger lock for this one operation.
    ///
    /// # Errors
    /// [`wire::Error::HeldUnclaimed`] when `id` is not parked under both
    /// `request` and `reply`.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_unpark(&self, id: DispatchId, request: RequestId, reply: KindId) -> Result<(), wire::Error> {
        self.inflight.unpark(id, request, reply)
    }

    /// The kind name of the context stored under `request` while it still
    /// carries a parked `Held` (ADR-0243 §7). Takes the ledger lock for this
    /// one read only while some entry is parked, so an actor that never
    /// parks takes no lock on its reply path.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn parked_context(&self, request: RequestId) -> Option<&'static str> {
        self.inflight.parked_context(request)
    }

    /// The named ledger entry's state, for tests.
    #[cfg(test)]
    pub(crate) fn dispatch_state_of(&self, id: DispatchId) -> Option<&'static str> {
        self.inflight.lock().dispatch_state_of(id)
    }
}
