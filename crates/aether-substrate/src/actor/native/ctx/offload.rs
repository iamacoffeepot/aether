//! Moving work off the actor's own thread.
//!
//! Three shapes, all ADR-0080 §12 / ADR-0093. A raw worker thread
//! (`spawn_inherit` / `spawn_detached`) runs a closure that sends nothing,
//! either holding this handler's causal chain open or holding none. A
//! staged task (`stage_blocking*`, ADR-0243 §9) owes no reply: staging takes
//! the settlement hold on this turn's chain and parks it in the per-actor
//! in-flight ledger, the task starts when its stager says, and its
//! completion runs correlated to the task's request on that chain. A
//! hold-until-resolve dispatch (`dispatch_blocking*`) acquires the
//! settlement hold the same way, but arms a reply to the current caller and
//! replies from a later handler turn when the completion wake lands.

use std::ptr;
use std::sync::{Arc, Weak};
use std::thread::{Builder as ThreadBuilder, JoinHandle};

use aether_actor::{Addressable, ErasedActorRef, HeldReply, ReplyMode, Singleton};
use aether_data::{ActorMail, Kind, RequestId};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::offload::blocking::{DeferredCompletion, DeferredReply, DispatchId, Pending, TaskDone};
use crate::actor::native::offload::fail_fast;
use crate::actor::native::offload::held::{Held, answer_unanswered};
use crate::actor::native::offload::self_wake::SelfWake;
use crate::actor::native::offload::staged_task::StagedTask;
use crate::actor::native::offload::thread;
use crate::mail::Source;
use crate::runtime::trace::SettlementHold;

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// ADR-0080 §12 spawn primitive: run `f` on a worker thread named for
    /// this actor, holding this handler's in-flight chain open until the
    /// worker exits.
    ///
    /// The settlement hold is acquired on this thread before the worker
    /// spawns and moves into the worker's body, so the chain cannot settle
    /// between this handler's `Finished` and the worker's exit. An absent
    /// [`Self::in_flight_root`] has no chain to hold, and the worker runs
    /// holding nothing. The worker receives no ctx and sends no mail: a
    /// thread that has to reach this actor again wakes it through
    /// [`Self::self_wake`], and work that replies in a later handler turn
    /// uses [`Self::dispatch_blocking`].
    ///
    /// Use for short-burst CPU offload that is *part of* the current
    /// handler's causal closure. For a worker that answers to no chain, use
    /// [`Self::spawn_detached`].
    ///
    /// A panic in `f` is fatal (ADR-0063): the worker escalates it through
    /// the chassis aborter with the panic payload in the reason, after its
    /// settlement hold has dropped.
    pub fn spawn_inherit(&self, f: impl FnOnce() + Send + 'static) -> JoinHandle<()>
    where
        A: Addressable,
    {
        thread::spawn_inherit(self.binding, self.in_flight_root, A::NAMESPACE, f)
    }

    /// ADR-0080 §12 spawn primitive: run `f` on a worker thread named for
    /// this actor, holding no chain. The worker receives no ctx and sends no
    /// mail.
    ///
    /// Use for a long-lived worker that answers to something outside any
    /// mail chain, such as a child process's pipe. For short-burst CPU
    /// offload that is part of the current handler's causal closure, use
    /// [`Self::spawn_inherit`].
    ///
    /// A panic in `f` is fatal (ADR-0063): the worker escalates it through
    /// the chassis aborter with the panic payload in the reason.
    pub fn spawn_detached(&self, f: impl FnOnce() + Send + 'static) -> JoinHandle<()>
    where
        A: Addressable + Singleton,
    {
        thread::spawn_detached(self.binding, A::NAMESPACE, f)
    }

    /// A [`SelfWake<K>`] for the rare dedicated thread a cap runs itself —
    /// a socket reader, a heartbeat, a backoff timer — to wake this actor
    /// with one `K` when it has staged work. The thread holds the handle in
    /// place of a stored mailbox id plus a mailer: it names no position and
    /// can send only that one wake (ADR-0230).
    #[must_use]
    pub fn self_wake<K: ActorMail>(&self) -> SelfWake<K> {
        SelfWake::new(self.binding)
    }

    /// Stage blocking work that owes no reply (ADR-0243 §9): mint its
    /// request id from the counter outbound requests use, take the
    /// settlement hold on this turn's chain (the in-flight root, or the
    /// causing chain of a `wire` ctx), and arm a ledger entry that owes
    /// nothing. The chain is fixed here; [`StagedTask::start`] only spawns
    /// the worker, so work staged in one request's turn and started from
    /// another's still holds the first request's chain.
    ///
    /// The `#[handler(task)]` completion runs correlated to the task's
    /// request, on that chain: its sends inherit it, `ctx.in_reply_to()` is
    /// [`StagedTask::request`], and its `TaskDone<O>` is discharged with
    /// `into_output`. A task that carries a context stages with
    /// [`Self::stage_blocking_with`].
    pub fn stage_blocking<O: Send + 'static>(&mut self) -> StagedTask<O> {
        let request = RequestId(self.binding.mint_correlation());
        StagedTask::new(request, self.binding.dispatch_stage(self.acquire_settlement_hold(), request))
    }

    /// [`Self::stage_blocking`] with a context: `context` is stored in the
    /// request-context table under the task's request id, as
    /// `send_with_context` stores a request's, and the completion takes it
    /// with `ctx.take_context::<C>()`. A completion that leaves a context
    /// holding a live `Held` untaken fails fast (ADR-0243 §7). Dropping the
    /// task unstarted removes the context.
    pub fn stage_blocking_with<O: Send + 'static, C: Kind>(&mut self, context: C) -> StagedTask<O> {
        let request = RequestId(self.binding.mint_correlation());
        self.binding.store_request_context(request, context);
        StagedTask::new(request, self.binding.dispatch_stage(self.acquire_settlement_hold(), request))
            .with_context::<C>(self.binding)
    }

    /// ADR-0093 hold-until-resolve dispatch: run the blocking closure
    /// `f` on a worker thread and reply to the current caller in a
    /// *later* handler turn, when the worker's output lands. Work that owes
    /// no reply stages with [`Self::stage_blocking`] instead.
    ///
    /// The settlement hold is acquired **eagerly on this thread, before
    /// the worker spawns** (so `HoldOpen` precedes this handler's
    /// `Finished` and the #716 premature-settlement window is closed by
    /// construction), then parked in the per-actor in-flight ledger
    /// alongside the originating [`Source`] — it outlives the worker
    /// (which holds nothing) and releases only when the completion is
    /// resolved. An absent [`Self::in_flight_root`] skips the
    /// hold cleanly (no chain to hold), matching `spawn_inherit`.
    ///
    /// When `f` returns, the worker stores the output in the ledger's
    /// completion slot and pushes a
    /// [`TaskCompletionWake`](crate::actor::native::offload::blocking::TaskCompletionWake) to this
    /// actor's own mailbox (the loopback-wake mechanism). The actor's
    /// completion handler decodes that wake's [`DispatchId`] and calls
    /// [`Self::take_task_done`] to rebuild the [`TaskDone`], then
    /// `resolve`s it.
    ///
    /// A panic in `f` is fatal (ADR-0063): the worker escalates it through
    /// the chassis aborter with the panic payload in the reason, exactly as
    /// the scheduler escalates a handler panic, and no completion lands. An
    /// expected failure belongs in the output value `O` (a `Result`-shaped
    /// output the completion maps to an error reply), never in a panic.
    ///
    /// Returns a [`Pending<R>`] (ADR-0109) — the type-level receipt a
    /// request handler returns to declare `-> Pending<R>`, naming `R` as
    /// the reply kind the matching `#[handler(task)]` completion sends.
    /// The inner [`DispatchId`] is reachable via [`Pending::dispatch_id`]
    /// for *optional* cancellation; the happy path ignores it. `R` is
    /// independent of the worker output `O` — the completion handler maps
    /// `O` to the reply `R` it returns.
    pub fn dispatch_blocking<O, R, F>(&mut self, f: F) -> Pending<R>
    where
        O: Send + 'static,
        R: ActorMail,
        F: FnOnce() -> O + Send + 'static,
    {
        self.dispatch_blocking_with_pending::<O, R, (), F>((), f)
    }

    /// Context-carrying variant of [`Self::dispatch_blocking`]: dispatches
    /// as [`Self::dispatch_blocking_with`] does, parking `cx` for the
    /// completion, and returns the [`Pending<R>`] receipt for the armed
    /// dispatch (ADR-0109, ADR-0243 §3). Work that owes no reply stages with
    /// [`Self::stage_blocking_with`] instead.
    pub fn dispatch_blocking_with_pending<O, R, C, F>(&mut self, cx: C, f: F) -> Pending<R>
    where
        O: Send + 'static,
        R: ActorMail,
        C: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        Pending::new(self.dispatch_blocking_with::<O, C, F>(cx, f))
    }

    /// Dispatch a blocking closure that answers an already-held reply
    /// (ADR-0243 §3): the worker attaches to the ledger entry `held` names,
    /// which keeps the settlement hold and reply target it captured, and
    /// parks `cx` for the completion. The completion replies to the caller
    /// the ticket was taken from, on the chain it kept open, and the
    /// returned id is the one the ticket's [`Pending<R>`] receipt carries.
    ///
    /// A bounded queue does not use this: it holds each request's reply
    /// with [`Self::hold`], keeps the [`Held`] itself, and stages the work
    /// with [`Self::stage_blocking`] in the request's own turn (ADR-0243 §9).
    ///
    /// # Panics
    /// Panics when `held` belongs to another actor, and when its entry is
    /// no longer held.
    pub fn dispatch_blocking_held_with<O, R, C, F>(&mut self, held: Held<R>, cx: C, f: F) -> DispatchId
    where
        O: Send + 'static,
        R: ActorMail,
        C: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        assert!(
            self.owns_ledger(held.ledger()),
            "a Held dispatched from another actor's ctx: a held reply is answered on the actor that armed it (ADR-0243 §5)"
        );
        let completion = self.binding.dispatch_attach_worker(held.into_ticket(), Box::new(cx));
        self.spawn_blocking_worker(completion, f)
    }

    /// Context-carrying variant of [`Self::dispatch_blocking`]
    /// (ADR-0093 §5): parks `cx` in the in-flight ledger alongside the
    /// hold + reply target so the completion handler receives a
    /// [`TaskDone<O, C>`] whose [`TaskDone::context`] is `cx`. Use when
    /// the completion genuinely needs actor-thread state the pure worker
    /// shouldn't take.
    pub fn dispatch_blocking_with<O, C, F>(&mut self, cx: C, f: F) -> DispatchId
    where
        O: Send + 'static,
        C: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        // ADR-0093 §1 / ADR-0080 §12: acquire the hold on the current root
        // and capture the reply target from *this* handler, then hand them
        // to the resumed core. A handler turn with no in-flight root yields
        // no hold, and the dispatch it starts is then outside settlement
        // (ADR-0168 §2). A bounded `TaskQueue` instead holds the reply and
        // stages the work in the request's own turn, so a deferred request
        // keeps its own chain held and is answered from the queue.
        let hold = self.acquire_settlement_hold();
        let reply_to = self.reply_target();
        self.dispatch_blocking_resumed_with(hold, reply_to, cx, f)
    }
    /// ADR-0093: dispatch a blocking closure with an externally-supplied
    /// `(hold, reply_to)` — *moved in* rather than read from this ctx.
    /// [`Self::dispatch_blocking`] is sugar over this that supplies them
    /// from the current handler. A caller that captured the hold + reply
    /// target when a request was accepted replays them here when the
    /// request finally dispatches from a later handler turn — so the
    /// deferred work keeps its *own* chain held and replies to its *own*
    /// caller, not the completion handler's. A bounded queue holds a
    /// [`Held`] instead and stages its work with [`Self::stage_blocking`].
    pub fn dispatch_blocking_resumed<O, F>(
        &mut self,
        hold: Option<SettlementHold>,
        reply_to: Source,
        f: F,
    ) -> DispatchId
    where
        O: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        self.dispatch_blocking_resumed_with(hold, reply_to, (), f)
    }

    /// Context-carrying core of the resumed dispatch. Inserts the ledger
    /// entry with the supplied `(hold, reply_to, cx)` and spawns the
    /// worker that runs `f`, parks its output, and wakes the actor.
    pub fn dispatch_blocking_resumed_with<O, C, F>(
        &mut self,
        hold: Option<SettlementHold>,
        reply_to: Source,
        cx: C,
        f: F,
    ) -> DispatchId
    where
        O: Send + 'static,
        C: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        let completion = self.binding.dispatch_arm(hold, reply_to, cx);
        self.spawn_blocking_worker(completion, f)
    }

    /// The single worker spawn site for every `dispatch_blocking*` path and
    /// for [`StagedTask::start`]: spawn the worker that runs `f`, fills
    /// `completion`'s ledger entry with the output, and wakes the actor.
    /// Returns the entry's id.
    pub(crate) fn spawn_blocking_worker<O, F>(&self, completion: DeferredCompletion<O>, f: F) -> DispatchId
    where
        O: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        let id = completion.dispatch_id();
        let aborter = self.binding.fatal_aborter();

        // The worker captures the binding + dispatch id, runs the
        // blocking closure, parks its output in the ledger, then pushes
        // the completion-wake to the actor's own mailbox. It touches no
        // actor state beyond the ledger slot it owns and dies after the
        // push. A panic in the closure is fatal (ADR-0063): the worker
        // escalates it through the aborter it holds strongly, even when the
        // owning actor has already closed, and leaves the completion armed;
        // an aborter that unwinds rather than exits drops it, which abandons
        // the entry and releases the hold. This is the one sanctioned raw
        // spawn for the hold-until-resolve shape (ADR-0093) — umbrella-aware because
        // the hold (held in the ledger, not here) keeps the chain open
        // until the resolve. The per-request spawn is a placeholder; the
        // scalable form is a reused work-stealing blocking pool isolated
        // from the cooperative scheduler (#1322).
        // This IS the ADR-0093 dispatch_blocking primitive — the hold lives in the
        // ledger (not on this worker), so the chain stays open until the resolve.
        #[allow(clippy::disallowed_methods)]
        let spawned = ThreadBuilder::new().name(String::from("aether-dispatch-blocking")).spawn(move || {
            completion.complete(fail_fast::run_or_abort(aborter.as_ref(), "dispatch_blocking worker", f));
        });
        if let Err(e) = spawned {
            tracing::error!(
                target: "aether_substrate::actor::native::offload::blocking",
                error = %e,
                "failed to spawn dispatch_blocking worker thread",
            );
            // Dropping the rejected worker closure abandons its armed
            // completion and releases the hold. Keep this explicit abandon
            // as a defensive miss: it is harmless after the token's Drop and
            // protects this path if thread-spawn ownership semantics change.
            drop(self.binding.dispatch_abandon(id));
        }
        id
    }

    /// Capture the current root as a reply this actor still owes, directing
    /// the eventual terminal reply to an explicitly carried target. The
    /// returned [`DeferredReply`] keeps the caller's chain open until it is
    /// replied to, staged onto a successor, or abandoned.
    pub fn defer_reply_to(&self, reply_to: Source) -> DeferredReply {
        DeferredReply::new(self.acquire_settlement_hold(), reply_to)
    }

    /// Arm a reply of kind `R` this handler answers later (ADR-0243 §1): the
    /// current settlement hold and reply target, as
    /// `defer_reply_to(reply_target())` captures them, parked in one
    /// in-flight ledger entry that no worker answers.
    ///
    /// The handler returns the [`Pending<R>`] receipt, which declares its
    /// row `-> Pending<R>`, and keeps the [`Held<R>`] debt in state or on a
    /// successor until [`Held::answer`] sends the one `R`. When the actor
    /// closes first while the engine keeps running, the close tail sends
    /// [`HeldReply::unanswered`] in its place, which is why `R` must
    /// implement [`HeldReply`]; an engine teardown settles it silently.
    ///
    /// # Panics
    /// Panics on a second `hold` in one dispatch (ADR-0243 §7): two debts
    /// on one request would send two replies.
    pub fn hold<R: HeldReply>(&mut self) -> (Pending<R>, Held<R>) {
        self.claim_dispatch_debt();
        let (id, ledger) =
            self.binding.dispatch_hold(self.acquire_settlement_hold(), self.reply_target(), answer_unanswered::<R>);
        (Pending::new(id), Held::new(id, ledger))
    }

    /// Owe one reply of kind `R` to this request's caller without holding
    /// its chain (ADR-0243 §1): [`Self::hold`] for a request whose caller
    /// must not wait. The entry carries the caller's reply target and no
    /// settlement hold, so the caller's chain settles as soon as this
    /// handler returns.
    ///
    /// [`Held::answer`] later replies to the captured caller with its
    /// correlation, so the caller's response handler gets the context it
    /// bound, and the reply joins no chain: it records no `Sent` and the
    /// receiving turn's own sends start fresh chains. Everything else is as
    /// for `hold`: the handler returns the [`Pending<R>`] receipt, a worker
    /// can answer through [`Self::dispatch_blocking_held_with`], an
    /// unanswered ticket fails fast on drop, and an actor close sends
    /// [`HeldReply::unanswered`].
    ///
    /// # Panics
    /// Panics on a second `hold` or `defer` in one dispatch (ADR-0243 §7):
    /// two debts on one request would send two replies.
    pub fn defer<R: HeldReply>(&mut self) -> (Pending<R>, Held<R>) {
        self.claim_dispatch_debt();
        let (id, ledger) = self.binding.dispatch_hold(None, self.reply_target(), answer_unanswered::<R>);
        (Pending::new(id), Held::new(id, ledger))
    }

    /// Record that this dispatch owes its one reply through a `Held`
    /// ([`Self::hold`] or [`Self::defer`]), failing fast on a second.
    fn claim_dispatch_debt(&mut self) {
        assert!(
            !self.held_this_dispatch,
            "a second NativeCtx::hold or defer in one dispatch: one request owes one reply (ADR-0243 §7)"
        );
        self.held_this_dispatch = true;
    }

    /// The ledger half of [`Held::answer`]: claim the held entry `id` from
    /// this actor's ledger, send `reply` to its captured target under the
    /// root its hold keeps open, echoing the captured correlation, and then
    /// release the hold, so `Sent` precedes `Release` (ADR-0080 §12).
    ///
    /// # Panics
    /// Panics when `ledger` is another actor's binding, and when the entry
    /// is not held in this ledger, which is a second answer for one ticket.
    pub(crate) fn answer_held<R: ActorMail>(&mut self, id: DispatchId, ledger: &Weak<NativeBinding>, reply: &R) {
        assert!(
            self.owns_ledger(ledger),
            "Held::answer from another actor's ctx: a held reply answers on the actor that armed it (ADR-0243 §5)"
        );
        let (hold, reply_to) = self
            .binding
            .dispatch_claim_held(id)
            .expect("Held::answer found no held ledger entry: a ticket answers once (ADR-0243 §1)");
        self.reply_to_target(reply_to, reply, hold.as_ref().map(SettlementHold::root), None);
        drop(hold);
    }

    /// The ledger half of [`Held::hand_off`]: claim the held entry `id` from
    /// this actor's ledger, push `payload` to `target` with the entry's
    /// captured reply target pinned and its held root as the lineage, and
    /// then release the hold. The push takes its settlement count before the
    /// release, so the chain stays open until `target` answers.
    ///
    /// # Panics
    /// Panics when `ledger` is another actor's binding, and when the entry
    /// is not held in this ledger, which is a second use of one ticket.
    pub(crate) fn hand_off_held<K: ActorMail>(
        &mut self,
        id: DispatchId,
        ledger: &Weak<NativeBinding>,
        target: ErasedActorRef,
        payload: &K,
    ) {
        assert!(
            self.owns_ledger(ledger),
            "Held::hand_off from another actor's ctx: a held reply leaves only the actor that armed it (ADR-0243 §5)"
        );
        let (hold, reply_to) = self
            .binding
            .dispatch_claim_held(id)
            .expect("Held::hand_off found no held ledger entry: a ticket is used once (ADR-0243 §1)");
        self.push_handed_off(target, payload, hold.as_ref().map(SettlementHold::root), reply_to);
        drop(hold);
    }

    /// Whether `ledger` is this ctx's actor's in-flight ledger, the one a
    /// [`Held`] must name to be answered or dispatched here.
    fn owns_ledger(&self, ledger: &Weak<NativeBinding>) -> bool {
        ptr::eq(ledger.as_ptr(), Arc::as_ptr(self.binding))
    }

    /// ADR-0093 completion-routing entry point: remove the in-flight
    /// ledger entry named by `id` (decoded from a landed
    /// [`TaskCompletionWake`](crate::actor::native::offload::blocking::TaskCompletionWake) and rebuild its [`TaskDone<O, C>`]. The
    /// (future) `#[handler(task)]` macro — and, for now, a hand-wired
    /// completion handler — calls this and then `resolve`s the result.
    ///
    /// A staged task's settlement hold moves onto this ctx, which releases
    /// it when the completion handler ends, after the handler's sends are
    /// counted on the chain it keeps open (ADR-0243 §9).
    ///
    /// `None` for an unknown id (cancelled or double-landed) or an `O` /
    /// `C` that doesn't match the dispatch's types (a wiring bug).
    pub fn take_task_done<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        self.binding.dispatch_take::<O, C>(id).map(|done| self.keep_task_hold(done))
    }

    /// Non-consuming sibling of [`Self::take_task_done`]: probe the
    /// in-flight entry named by `id` against `O` / `C` and only remove +
    /// rebuild the [`TaskDone<O, C>`] on a match, leaving the entry intact
    /// on a mismatch.
    ///
    /// This is the routing primitive behind `#[handler(task)]`. Multiple
    /// task handlers on one actor are discriminated by their `TaskDone<O>`
    /// output type, not a kind id — all completions arrive as the single
    /// [`TaskCompletionWake`](crate::actor::native::offload::blocking::TaskCompletionWake) kind. The generated dispatch arm tries each
    /// task handler's `(O, C)` in
    /// turn; a wrong-type probe must *not* consume the entry, or the first
    /// handler tried would swallow a completion destined for a later one.
    ///
    /// `None` for an unknown id (cancelled / double-landed), an unfilled
    /// output, or an `O` / `C` that doesn't match this entry's dispatch.
    pub fn try_take_task_done<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        self.binding.dispatch_try_take::<O, C>(id).map(|done| self.keep_task_hold(done))
    }

    /// Move a staged task's settlement hold from its completion onto this
    /// ctx, whose drop releases it after the handler-end flush.
    fn keep_task_hold<O, C>(&mut self, mut done: TaskDone<O, C>) -> TaskDone<O, C> {
        if let Some(hold) = done.take_task_hold() {
            self.task_holds.push(hold);
        }
        done
    }
}
