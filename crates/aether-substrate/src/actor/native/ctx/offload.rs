//! Moving work off the actor's own thread.
//!
//! Two shapes, both ADR-0080 §12 / ADR-0093. A raw worker thread
//! (`spawn_inherit` / `spawn_detached`) either folds into this handler's
//! causal chain or deliberately starts its own. A hold-until-resolve
//! dispatch (`dispatch_blocking*`) acquires the settlement hold eagerly on
//! this thread, parks it in the per-actor in-flight ledger, and replies from
//! a later handler turn when the completion wake lands.

use std::sync::Arc;
use std::thread::{Builder as ThreadBuilder, JoinHandle};

use aether_actor::{Addressable, ReplyMode, Singleton};
use aether_data::Kind;

use crate::actor::native::offload::blocking::{DeferredCompletion, DeferredReply, DispatchId, Pending, TaskDone};
use crate::actor::native::offload::thread;
use crate::actor::native::{InheritCtx, RootCtx};
use crate::mail::Source;
use crate::runtime::trace::SettlementHold;

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, M, A> {
    /// ADR-0080 §12 spawn primitive: launch a worker thread that
    /// inherits this handler's in-flight `(mail_id, root)` so its
    /// sends fold into the current causal chain. The closure `f`
    /// receives a [`InheritCtx<W>`] — sends
    /// from inside `f` carry `parent_mail = self.in_flight_mail_id()`
    /// and `root = self.in_flight_root()` automatically.
    ///
    /// Use for short-burst CPU offload that is *part of* the current
    /// handler's causal closure (e.g., parsing, encoding,
    /// pixel-pushing). For long-lived workers responding to external
    /// events with no caller context (TCP per-connection workers,
    /// pollers), use [`Self::spawn_detached`] instead.
    ///
    /// **Settlement contract gap (issue iamacoffeepot/aether#716):**
    /// the parent chain may settle before the worker's first send
    /// arrives; callers gate-sensitive to settlement should not
    /// rely on the parent chain staying open for the worker's
    /// lifetime today.
    pub fn spawn_inherit<W, F>(&self, f: F) -> JoinHandle<()>
    where
        // ADR-0119: `W` only supplies `W::NAMESPACE` (thread name) and
        // parameterizes `InheritCtx<W>` (Addressable-only). The former
        // `Singleton` bound was incidental, and single-cardinality
        // enforcement made it block instanced workers — relaxed to Addressable.
        W: Addressable + 'static,
        F: FnOnce(InheritCtx<W>) + Send + 'static,
    {
        thread::spawn_inherit::<W, F>(Arc::clone(self.binding), self.in_flight_mail_id, self.in_flight_root, f)
    }

    /// ADR-0080 §12 spawn primitive: launch a worker thread with no
    /// in-flight inheritance. The closure `f` receives a
    /// [`RootCtx<W>`] — each send mints a
    /// fresh root chain with `W`'s mailbox as the producer.
    ///
    /// Use for long-lived workers that respond to external events
    /// (TCP per-connection workers, pollers). For short-burst CPU
    /// offload that is part of the current handler's causal closure,
    /// use [`Self::spawn_inherit`].
    pub fn spawn_detached<W, F>(&self, f: F) -> JoinHandle<()>
    where
        W: Addressable + Singleton + 'static,
        F: FnOnce(RootCtx<W>) + Send + 'static,
    {
        thread::spawn_detached::<W, F>(Arc::clone(self.binding), f)
    }

    /// ADR-0093 hold-until-resolve dispatch: run the blocking closure
    /// `f` on a worker thread and reply to the current caller in a
    /// *later* handler turn, when the worker's output lands.
    ///
    /// The settlement hold is acquired **eagerly on this thread, before
    /// the worker spawns** (so `HoldOpen` precedes this handler's
    /// `Finished` and the #716 premature-settlement window is closed by
    /// construction), then parked in the per-actor in-flight ledger
    /// alongside the originating [`Source`] — it outlives the worker
    /// (which holds nothing) and releases only when the completion is
    /// resolved. `MailId::NONE` for [`Self::in_flight_root`] skips the
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
        R: Kind,
        F: FnOnce() -> O + Send + 'static,
    {
        let id = self.dispatch_blocking_with::<O, (), F>((), f);
        Pending::new(id)
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
        // (ADR-0168 §2). A bounded `TaskQueue`
        // instead captures `(hold, reply_to)` at accept time and replays
        // them via `dispatch_blocking_resumed` when a slot frees, so a
        // deferred request keeps its own chain held and replies to its own
        // caller.
        let hold = self.acquire_settlement_hold();
        let reply_to = self.reply_target();
        self.dispatch_blocking_resumed_with(hold, reply_to, cx, f)
    }
    /// ADR-0093: dispatch a blocking closure with an externally-supplied
    /// `(hold, reply_to)` — *moved in* rather than read from this ctx.
    /// [`Self::dispatch_blocking`] is sugar over this that supplies them
    /// from the current handler. The bound/queue path (`TaskQueue`)
    /// captures the hold + reply target when a request is accepted and
    /// replays them here when the request finally dispatches from a later
    /// handler turn — so the deferred work keeps its *own* chain held and
    /// replies to its *own* caller, not the completion handler's.
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

    /// Context-carrying core of the resumed dispatch — the single worker
    /// spawn site for every `dispatch_blocking*` path. Inserts the ledger
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
        let id = completion.dispatch_id();

        // The worker captures the binding + dispatch id, runs the
        // blocking closure, parks its output in the ledger, then pushes
        // the completion-wake to the actor's own mailbox. It touches no
        // actor state beyond the ledger slot it owns and dies after the
        // push. This is the one sanctioned raw spawn for the
        // hold-until-resolve shape (ADR-0093) — umbrella-aware because
        // the hold (held in the ledger, not here) keeps the chain open
        // until the resolve. The per-request spawn is a placeholder; the
        // scalable form is a reused work-stealing blocking pool isolated
        // from the cooperative scheduler (#1322).
        // This IS the ADR-0093 dispatch_blocking primitive — the hold lives in the
        // ledger (not on this worker), so the chain stays open until the resolve.
        #[allow(clippy::disallowed_methods)]
        let spawned = ThreadBuilder::new().name(String::from("aether-dispatch-blocking")).spawn(move || {
            completion.complete(f());
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

    /// Arm the shared ADR-0093 ledger for a typed deferred producer without
    /// tying completion to a blocking worker. Future staged effects can move
    /// this capability to their authoritative owner and retain no parent
    /// lifetime beyond a weak binding reference.
    ///
    /// The armed completion carries whatever hold this context can give it —
    /// the in-flight root of a dispatching handler, or the causing chain of a
    /// `wire` hook (ADR-0168 §1). It carries none where the context has
    /// neither, and the staged effect is then outside settlement entirely.
    pub(crate) fn arm_deferred_completion<O, C>(&self, context: C) -> DeferredCompletion<O>
    where
        C: Send + 'static,
    {
        self.binding.dispatch_arm(self.acquire_settlement_hold(), self.reply_target(), context)
    }
    /// Capture the current root as a reply this actor still owes, directing
    /// the eventual terminal reply to an explicitly carried target. The
    /// returned [`DeferredReply`] keeps the caller's chain open until it is
    /// replied to, staged onto a successor, or abandoned.
    pub fn defer_reply_to(&self, reply_to: Source) -> DeferredReply {
        DeferredReply::new(self.acquire_settlement_hold(), reply_to)
    }

    /// ADR-0093 completion-routing entry point: remove the in-flight
    /// ledger entry named by `id` (decoded from a landed
    /// [`TaskCompletionWake`](crate::actor::native::offload::blocking::TaskCompletionWake) and rebuild its [`TaskDone<O, C>`]. The
    /// (future) `#[handler(task)]` macro — and, for now, a hand-wired
    /// completion handler — calls this and then `resolve`s the result.
    ///
    /// `None` for an unknown id (cancelled or double-landed) or an `O` /
    /// `C` that doesn't match the dispatch's types (a wiring bug).
    pub fn take_task_done<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        self.binding.dispatch_take::<O, C>(id)
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
        self.binding.dispatch_try_take::<O, C>(id)
    }
}
