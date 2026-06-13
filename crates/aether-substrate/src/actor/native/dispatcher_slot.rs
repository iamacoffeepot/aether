//! [`DispatcherSlot<A>`] — the [`Drainable`] adapter that wraps a
//! native actor for chassis worker-pool dispatch (issue 635 PR C).
//!
//! ## The dispatch cycle
//!
//! `DispatcherSlot::run_cycle` is the *budget-bounded* dispatch body the
//! chassis worker pool runs against this slot. Each call to `run_cycle`
//! does:
//!
//! 1. CAS `Ready → Running` on the [`SlotState`] (caller invariant:
//!    the slot was just popped from the ready queue).
//! 2. Drains envelopes via [`NativeBinding::try_recv`] until
//!    inbox is empty, the budget is exhausted, or shutdown fires.
//!    Per-envelope wrapping is `local::with_stamped(slots, ...)` +
//!    `log_install::with_actor_dispatch(binding, ...)` so traces /
//!    `Local<T>` lookups behave identically across every actor, and the
//!    per-envelope dispatch reuses the shared helpers in
//!    [`crate::actor::native::dispatch`].
//! 3. Returns one of:
//!    - [`CycleResult::Idle`] — inbox drained, post-empty recheck saw
//!      no race; worker drops the slot Arc.
//!    - [`CycleResult::Requeue`] — budget hit (state `Ready`) or
//!      post-empty recheck won the requeue CAS; worker re-pushes.
//!    - [`CycleResult::Closed`] — shutdown observed; the slot ran the
//!      post-shutdown drain + `unwire` hook + registry finalize
//!      sequence and is done forever.
//!
//! ## Sole dispatch path
//!
//! Every actor drains on the chassis worker pool (issue 635 Phase 3 made
//! `Pooled` the default; issue 1187 removed the per-thread opt-out), so
//! this slot is the runtime dispatch path for every actor — chassis caps
//! and loaded wasm trampolines alike. `make_native_actor_boot` /
//! `Spawner::spawn_actor` construct the slot; the chassis worker pool
//! drives it.
//!
//! ## In-place demux seed (iamacoffeepot/aether#1135)
//!
//! [`Self::seize_and_run`] is the demux-direct counterpart to
//! [`Self::run_cycle`]: a [`crate::actor::native::blob_work::BlobWork`]
//! that has **seized** this slot (`Idle → Running`) hands it one
//! envelope to dispatch in place — skipping the inbox deposit +
//! `try_recv` repop the deposit-then-wake path paid. Both methods share
//! the same drain tail ([`Self::drain_after_seed`]).

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::actor::native::Envelope;
use crate::runtime::thread_name;
use aether_actor::local;
use aether_actor::local::ActorSlots;
use aether_kinds::trace::TraceEvent;
use std::ops::Deref;
use std::sync::PoisonError;

/// `ActorSlots` uses `RefCell` internally because the dedicated-thread
/// dispatcher path only ever reaches it from one OS thread. Worker-pool
/// dispatch can have *different* worker threads hit the same slot
/// across cycles, so the wrapper has to make those accesses sound.
///
/// The root guarantor is the actor [`Mutex`](DispatcherSlot::actor):
/// every read of the inner `ActorSlots` happens inside
/// [`DispatcherSlot::drain_after_seed`], which holds that lock for the
/// whole drain. The lock provides both the mutual exclusion (one
/// dispatcher body at a time) and the happens-before edge that
/// publishes one body's `RefCell` mutations to the next. The
/// [`SlotState`] machine is the *scheduling filter* layered above it —
/// it keeps the common case to a single un-contended worker — but it is
/// not the exclusion on its own: in the post-`mark_idle` recheck window
/// a worker can dispatch an envelope without holding `Running` while a
/// second worker legitimately enters `drain_after_seed`, so only the
/// actor `Mutex` actually serializes the `ActorSlots` access there.
#[repr(transparent)]
struct PooledSlots(Box<ActorSlots>);

// SAFETY: see the doc-comment on `PooledSlots`. Every access to the
// inner `ActorSlots` is made under the actor `Mutex` held across
// `DispatcherSlot::drain_after_seed`, which serializes the `RefCell`
// accesses and establishes the happens-before edge between successive
// dispatch bodies regardless of which worker thread runs them.
unsafe impl Sync for PooledSlots {}

impl Deref for PooledSlots {
    type Target = ActorSlots;
    fn deref(&self) -> &ActorSlots {
        &self.0
    }
}

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::{NativeActor, NativeDispatch};
use crate::actor::registry::ActorRegistry;
use crate::mail::mailer::Mailer;
use crate::mail::{KindId, Mail, MailboxId, Source};
use crate::scheduler::{
    BatchBudget, CLOCK_CHECK_STRIDE, CycleResult, Drainable, SeizeSeed, SlotState, burst_note_mail,
    time_budget,
};

/// Worker-pool-side wrapper for a native actor. One instance per
/// `Pooled` actor; held strongly by the chassis (so `unwire` and
/// registry finalize run when the cap shuts down) and weakly by the
/// pool's [`crate::scheduler::WakeHandle`] (so a wake after the cap
/// is gone silently no-ops).
pub struct DispatcherSlot<A>
where
    A: NativeActor + NativeDispatch,
{
    /// The slot's atomic state machine. Shared with the `WakeHandle`.
    pub(crate) state: Arc<SlotState>,
    /// The actor itself. This `Mutex` is the root mutual-exclusion +
    /// happens-before guarantor for a slot's dispatch: every drain runs
    /// under it (see [`Self::drain_after_seed`]), so two workers that
    /// reach the slot — e.g. a recheck-window dispatch racing a fresh
    /// `seize_and_run` — serialize here rather than relying on
    /// [`SlotState`] alone, which is the scheduling filter above it.
    /// `Option` so the `Closed` finalize path can take the box and run
    /// `unwire` on the consumed actor.
    actor: Mutex<Option<Box<A>>>,
    /// Per-actor binding (inbox + shutdown flag + reply machinery).
    binding: Arc<NativeBinding>,
    /// Per-actor `Local<T>` storage. Stamped into TLS for each
    /// envelope dispatch. Wrapped in [`PooledSlots`] for the `Sync`
    /// safety story — see that type's doc-comment.
    slots: PooledSlots,
    /// Chassis-level actor registry. Used by [`Self::finalize_registry`]
    /// to drain `monitors_of[id]` and prune `monitoring[id]` from each
    /// target on shutdown.
    actor_registry: Arc<ActorRegistry>,
    /// Mailer used to dispatch [`aether_kinds::MonitorNotice`] mail to
    /// any watchers when the slot finalizes.
    mailer: Arc<Mailer>,
    /// This slot's mailbox id — passed to `actor_registry.close_actor`.
    self_id: MailboxId,
    /// Static label for tracing / fairness logs. Today this is the
    /// actor's `NAMESPACE`.
    label: &'static str,
    /// Issue 714: one-shot completion sender installed by
    /// [`crate::actor::native::spawn::Spawner::shutdown_instanced`].
    /// Fired exactly once after the `Closed` branch of [`Self::run_cycle`]
    /// finishes its `unwire` + registry-close + `actor_guard.take()`
    /// sequence. The Spawner waits on the matching receiver via
    /// `recv_timeout` so chassis teardown settles deterministically
    /// without a 2 ms polling loop. `Mutex<Option<_>>` so the slot can
    /// take + send without holding the lock across the actor mutex.
    close_done_tx: Mutex<Option<crossbeam_channel::Sender<()>>>,
}

impl<A> DispatcherSlot<A>
where
    A: NativeActor + NativeDispatch,
{
    /// Borrow this slot's [`SlotState`] — needed by callers building a
    /// [`crate::scheduler::WakeHandle`] over the slot.
    pub(crate) fn state(&self) -> &Arc<SlotState> {
        &self.state
    }

    /// Borrow this slot's [`NativeBinding`]. The chassis-cap shutdown
    /// path uses this to call [`NativeBinding::signal_shutdown`] when
    /// the cap is going down — the next call into [`Self::run_cycle`]
    /// observes the flag and runs the `unwire` + registry finalize
    /// sequence.
    pub(crate) fn binding(&self) -> &Arc<NativeBinding> {
        &self.binding
    }

    pub(crate) fn new(
        actor: Box<A>,
        binding: Arc<NativeBinding>,
        slots: Box<ActorSlots>,
        actor_registry: Arc<ActorRegistry>,
        mailer: Arc<Mailer>,
        self_id: MailboxId,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(SlotState::new()),
            actor: Mutex::new(Some(actor)),
            binding,
            slots: PooledSlots(slots),
            actor_registry,
            mailer,
            self_id,
            label: A::NAMESPACE,
            close_done_tx: Mutex::new(None),
        })
    }

    /// Issue 714: fire the installed one-shot completion sender if any.
    /// Called once from the `Closed` branch of [`Self::run_cycle`] after
    /// `unwire` + registry close + `actor_guard.take()` have run. Take +
    /// `try_send`: bounded(1) guarantees the receiver only sees the
    /// first send; subsequent calls (idempotent — there should never be
    /// any) are no-ops. Done outside the actor mutex.
    fn fire_close_done(&self) {
        let tx = self
            .close_done_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(tx) = tx {
            // Receiver may have hung up if the wait already timed out.
            // Either way, the channel goes away after this call.
            let _ = tx.try_send(());
        }
    }

    /// Per-envelope dispatch over the shared helpers in
    /// [`crate::actor::native::dispatch`]. Wraps the dispatch call in
    /// `local::with_stamped` so per-actor `Local<T>` lookups (including
    /// the ADR-0081 `ActorLogRing`) resolve to this actor's slots.
    /// ADR-0081 retired the prior `log_install::with_actor_dispatch`
    /// wrap + per-handler flush hop.
    fn dispatch_one(&self, actor: &mut Box<A>, env: Envelope) {
        // iamacoffeepot/aether#1160: note this envelope against the
        // worker's local-drain burst *before* running the handler, so a
        // blob this handler produces (scheduled at `ctx` drop below) is
        // measured against a burst start that already covers this handler.
        // With the time valve on, the burst's first mail anchors the start
        // (one clock read per burst); with it off, this is a no-op.
        burst_note_mail(time_budget());
        // #1757: the single dispatched envelope moves into `ctx.inbound`
        // below, so read its `Copy` trace/settlement fields out first —
        // the `Received` / `Finished` / cost brackets and the settlement
        // tail run off these locals and never re-borrow the moved value.
        let mail_id = env.mail_id;
        let root = env.root;
        let kind = env.kind;
        let t_enqueue = env.t_enqueue;
        let enqueue_depth = env.enqueue_depth;
        let sender = env.sender;
        // Issue 734 / ADR-0088 §7: stamp the dispatching thread's
        // name-hashed `ThreadId` (a `Copy` u64) onto the `Received`
        // event. Resolved once per worker thread via a thread-local
        // cache — no per-hop `str::to_owned`, no `thread::current()`
        // `Arc` bump. The display name is recovered on the cold render
        // path through the reverse-lookup registry. Every actor drains on
        // the pool (issue 635 / issue 1187), so this is always the
        // worker's `aether-worker-N`.
        let thread_id = thread_name::current_thread_id();
        let inbound = local::with_stamped(&self.slots, || {
            // ADR-0086 Phase 3: `Received` / `Finished` land in this
            // (recipient) actor's trace ring — only inside this
            // `with_stamped` is its `ActorSlots` stamped.
            let th = self.binding.mailer().trace_handle();
            // iamacoffeepot/aether#1128: capture the `Received` instant
            // so the cost fold below reuses the existing trace bracket —
            // no new timestamp on the hot path.
            let t_received = th.now_nanos();
            th.push_trace_ring(
                root,
                TraceEvent::Received {
                    mail_id,
                    t: t_received,
                    // iamacoffeepot/aether#1134: surface the deposit
                    // instant + scheduler backlog the producer stamped at
                    // `route_mail`, so the hop splits into send→enqueue +
                    // queue residence.
                    t_enqueue,
                    enqueue_depth,
                    thread_id,
                },
            );
            // #1757 / ADR-0094: the dispatched envelope lives in exactly
            // one place — `ctx.inbound`. The dispatch arms read a disarmed
            // *view* (a clone whose obligation never fires), so the single
            // armed envelope settles exactly once: either the settlement
            // tail below discharges it, or a handler retained it via
            // `take_inbound`. A copied-metadata `take_inbound` against a
            // separately-held original would silently double-settle (both
            // disarm cleanly, the chain settles before the deferred reply,
            // the caller times out); single ownership makes that
            // unrepresentable.
            let view = env.clone();
            let mut ctx = NativeCtx::with_inbound(&self.binding, sender, mail_id, root, env);
            // ADR-0081 / ADR-0086 / iamacoffeepot/aether#1128
            // framework-built-in dispatch arms for `aether.log.tail` +
            // `aether.trace.tail` + `aether.cost.tail`. See the helper
            // docs in `dispatch`.
            if !super::dispatch::dispatch_log_tail_if_matching(&mut ctx, &view)
                && !super::dispatch::dispatch_trace_tail_if_matching(&mut ctx, &view)
                && !super::dispatch::dispatch_cost_tail_if_matching(&self.binding, &mut ctx, &view)
            {
                super::dispatch::typed_then_fallback_or_warn::<A>(actor, &mut ctx, &view);
            }
            // #1757: reclaim the single envelope before the ctx (and its
            // handler-end flush) drops, so an armed inbound is never
            // dropped *inside* the ctx — that would trip the ADR-0094
            // guard. `None` means a handler retained it via `take_inbound`.
            let inbound = ctx.take_raw_inbound();
            // iamacoffeepot/aether#1150: flush before `Finished` so a
            // child `Sent` (stamped at flush-begin on `ctx` drop) precedes
            // its parent's `Finished`.
            drop(ctx);
            let t_finished = th.now_nanos();
            th.push_trace_ring(
                root,
                TraceEvent::Finished {
                    mail_id,
                    t: t_finished,
                },
            );
            // iamacoffeepot/aether#1128: fold this handler's execution
            // time into its per-handler EWMA (lock-free through the
            // per-actor cache; framework / fallback kinds skipped).
            // Measure-only — no scheduling change. See
            // `dispatch::fold_handler_cost`.
            super::dispatch::fold_handler_cost(kind, t_received, t_finished);
            inbound
        });
        // #1757 / ADR-0080 §2 / ADR-0094: settle the single envelope
        // exactly once. `Some` is the normal path — `record_finished`
        // beside `discharge`, the canonical settle site every wasm
        // component and native actor drains through. `None` means a
        // handler retained the guard via `take_inbound`; its own un-fired
        // `record_finished` rides the retained `InboundMail` and closes
        // the chain when that guard drops, after its deferred reply.
        if let Some(env) = inbound {
            self.binding.mailer().record_finished(mail_id, root);
            env.discharge();
        }
    }

    /// The close hook in the slot teardown sequence. Wraps `actor.unwire`
    /// in `with_stamped` so any final tracing or `Local<T>` access from
    /// the close hook resolves to this actor's slots.
    fn run_close_hook(&self, actor: &mut Box<A>) {
        local::with_stamped(&self.slots, || {
            let mut close_ctx = NativeCtx::new(
                &self.binding,
                Source::NONE,
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            );
            actor.unwire(&mut close_ctx);
        });
    }

    /// Phase 4 — drain `monitors_of[self_id]`, prune `monitoring[id]`
    /// from each target, mark Dead, fan `MonitorNotice` mail out via
    /// the chassis mailer.
    fn finalize_registry(&self) {
        let watchers = self.actor_registry.close_actor(self.self_id);
        if !watchers.is_empty() {
            let notice = aether_kinds::MonitorNotice {
                target: self.self_id,
            };
            let payload =
                <aether_kinds::MonitorNotice as aether_data::Kind>::encode_into_bytes(&notice);
            let kind = KindId(<aether_kinds::MonitorNotice as aether_data::Kind>::ID.0);
            for watcher in watchers {
                self.mailer
                    .push(Mail::new(watcher, kind, payload.clone(), 1));
            }
        }
    }

    /// Shared drain tail for [`Drainable::run_cycle`] (no seed) and
    /// [`Drainable::seize_and_run`] (one direct-dispatch seed,
    /// iamacoffeepot/aether#1135). Caller invariant: the slot's
    /// [`SlotState`] is already `Running` — `run_cycle` won the
    /// `Ready → Running` CAS, `seize_and_run` won the `Idle → Running`
    /// seize — so this method owns the actor exclusively. It locks the
    /// actor, dispatches `seed` (if any) first, then runs the same drain
    /// loop + shutdown / budget / post-empty-recheck finalization both
    /// paths share, returning the [`CycleResult`].
    fn drain_after_seed(&self, seed: Option<Envelope>, budget: BatchBudget) -> CycleResult {
        let mut actor_guard = self.actor.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(actor) = actor_guard.as_mut() else {
            // Slot already finalized — the actor box was taken by the
            // `Closed` path. A `run_cycle` caller can't reach here (it
            // failed `enter_running` against the `Idle` a finalized slot
            // parks in), but a `seize_and_run` seed can race the narrow
            // window between `finalize`'s `actor_guard.take()` and the
            // strong slot Arc dropping: the `Idle → Running` seize wins
            // and the `Weak` still upgrades. Balance the seed's `Sent` so
            // its settlement chain still drains (ADR-0080 §2 — the same
            // bracket `route_mail`'s `Dropped` arm records), then drop it.
            if let Some(seed) = seed {
                self.binding
                    .mailer()
                    .record_finished(seed.mail_id, seed.root);
                // ADR-0094: discharge beside the finalized-slot seed's
                // `record_finished` — the seed is consumed (dropped)
                // here, never run.
                seed.discharge();
            }
            drop(actor_guard);
            self.state.mark_idle();
            // Issue 714: a wait that came in after the close cycle
            // already ran needs the signal too.
            self.fire_close_done();
            return CycleResult::Closed;
        };

        // iamacoffeepot/aether#1135: the demux-direct seed runs first,
        // in place — no inbox deposit, no `try_recv` repop. The seed's
        // `Received` carries `enqueue_depth = 0` and (iamacoffeepot/aether#1150)
        // `t_enqueue` = the blob-pickup stamp the `BlobWork` demuxer took at
        // `run_cycle` entry, so `t_received − t_enqueue` is the real in-blob
        // drain (pre-#1150 the pop-time stamp made it ≈ 0).
        if let Some(seed) = seed {
            self.dispatch_one(actor, seed);
        }

        let mut dispatched = 0u32;
        let mut cycle_start: Option<Instant> = None;
        let mut shutdown_observed = false;
        let mut budget_hit = false;
        let mut inbox_empty = false;
        loop {
            if self.binding.should_shutdown() {
                shutdown_observed = true;
                break;
            }
            let Some(env) = self.binding.try_recv() else {
                inbox_empty = true;
                break;
            };
            self.dispatch_one(actor, env);
            dispatched += 1;
            // Count cap: hard backstop, checked every dispatch with no
            // clock read (iamacoffeepot/aether#1067).
            if dispatched >= budget.max_mails {
                budget_hit = true;
                break;
            }
            // Time cap: only read the clock once batching past the
            // stride, so a warm single/few-mail cycle (which drains to
            // empty first) never touches the clock. The deadline is
            // measured from the first checked mail — a fairness
            // backstop, not a hard cycle deadline.
            if dispatched.is_multiple_of(CLOCK_CHECK_STRIDE) {
                let start = *cycle_start.get_or_insert_with(Instant::now);
                if start.elapsed() >= budget.max_dur {
                    budget_hit = true;
                    break;
                }
            }
        }

        if shutdown_observed {
            // Phase 2: drain residual inbox synchronously.
            while let Some(env) = self.binding.try_recv() {
                self.dispatch_one(actor, env);
            }
            // Phase 3: unwire hook.
            self.run_close_hook(actor);
            // Phase 4: registry close + monitor fan-out.
            self.finalize_registry();
            actor_guard.take();
            // Drop the actor mutex before signalling so the waiter (the
            // chassis-teardown thread in `Spawner::shutdown_instanced`)
            // wakes onto an unlocked slot.
            drop(actor_guard);
            self.state.mark_idle();
            // Issue 714: signal chassis teardown that this slot's
            // close cycle finished. `is_closed()` would return `true`
            // from this point onward; the channel signal lets the
            // waiter wake immediately instead of polling.
            self.fire_close_done();
            return CycleResult::Closed;
        }

        if budget_hit {
            self.state.mark_ready();
            return CycleResult::Requeue;
        }

        // Inbox observed empty. Post-empty recheck — close the
        // classic send-vs-drain race. After `mark_idle`, a fresh send
        // from a peer arrives in one of two timelines:
        //
        // (a) Sender pushes BEFORE our `mark_idle`: their `try_wake`
        //     fails (state still `Running`); they skip the requeue.
        //     Our `try_recv` after `mark_idle` finds the envelope; we
        //     CAS `Idle → Ready`; we requeue.
        //
        // (b) Sender pushes AFTER our `mark_idle`: their `try_wake`
        //     wins; they push the slot to the ready queue. Our CAS
        //     `Idle → Ready` fails (state is `Ready` now). The slot
        //     is already requeued — we return `Idle`.
        debug_assert!(inbox_empty);
        self.state.mark_idle();
        // match arms read clearer than `map_or_else(|| ..., |env| ...)` here
        // because the Some arm runs multi-line side effects.
        #[allow(clippy::option_if_let_else)]
        match self.binding.try_recv() {
            Some(env) => {
                self.dispatch_one(actor, env);
                if self.state.try_self_requeue() {
                    CycleResult::Requeue
                } else {
                    CycleResult::Idle
                }
            }
            None => CycleResult::Idle,
        }
    }
}

impl<A> Drainable for DispatcherSlot<A>
where
    A: NativeActor + NativeDispatch,
{
    fn run_cycle(&self, budget: BatchBudget) -> CycleResult {
        if !self.state.enter_running() {
            // Invariant violation: the worker popped this slot and
            // its state should have been Ready. Defensive fallback
            // — bail without touching the actor.
            tracing::warn!(
                target: "aether_substrate::scheduler",
                actor = A::NAMESPACE,
                "DispatcherSlot::run_cycle entered without Ready state — skipping",
            );
            return CycleResult::Idle;
        }
        // State is `Running`; drain the inbox with no seed.
        self.drain_after_seed(None, budget)
    }

    /// iamacoffeepot/aether#1135: dispatch one direct-dispatch `seed` in
    /// place, then drain the rest of the inbox. Caller invariant: the
    /// demuxer just won this slot's [`SlotState::seize`] CAS
    /// (`Idle → Running`), so the slot is `Running` and exclusively ours
    /// — no `enter_running` here (it would fail against `Running`). The
    /// drain tail is shared with [`Self::run_cycle`] via
    /// [`Self::drain_after_seed`].
    fn seize_and_run(&self, seed: SeizeSeed, budget: BatchBudget) -> CycleResult {
        self.drain_after_seed(Some(seed), budget)
    }

    fn label(&self) -> &'static str {
        self.label
    }

    /// Issue 685: chassis-teardown signal. Forwards to the binding's
    /// `signal_shutdown` so the next [`Self::run_cycle`] observes
    /// `should_shutdown` at the top of its drain loop and runs the
    /// close path (phases 2-4 already implemented). Spawner walks
    /// every instanced slot at chassis teardown and calls this before
    /// firing a wake.
    fn signal_shutdown(&self) {
        self.binding.signal_shutdown();
    }

    /// Issue 685: chassis-teardown wait predicate. The Closed branch
    /// of [`Self::run_cycle`] takes the actor out of the `Mutex<Option<Box<A>>>`
    /// guard, so `actor_guard.is_none()` is equivalent to "close cycle
    /// has run." Issue 714 retired the polling caller in favour of a
    /// channel signal (see [`Self::set_close_done_tx`]), but the
    /// predicate stays available for diagnostics + the fast-path
    /// already-closed check inside `set_close_done_tx`.
    fn is_closed(&self) -> bool {
        let guard = self.actor.lock().unwrap_or_else(PoisonError::into_inner);
        guard.is_none()
    }

    /// Issue 714: install the chassis-teardown completion sender.
    /// Stash it in the slot; the close cycle's `fire_close_done` will
    /// fire it on the way out. Fast path: if the slot already finished
    /// its close cycle (actor mutex empty), fire immediately so a late
    /// waiter doesn't park forever waiting for a signal that already
    /// passed.
    fn set_close_done_tx(&self, tx: crossbeam_channel::Sender<()>) {
        // Fast-path: already closed. Signal directly without stashing.
        if self.is_closed() {
            let _ = tx.try_send(());
            return;
        }
        let prior = self
            .close_done_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(tx);
        // Defensive: if a prior sender was installed (shouldn't happen
        // — `shutdown_instanced` runs once per chassis), drop it. The
        // bounded(1) channel goes away with it; that waiter will see
        // a Disconnected, not a Timeout.
        drop(prior);
        // Re-check: the close cycle may have run between the
        // `is_closed` fast-path check and the stash. If so, fire the
        // sender we just stashed manually — it isn't going to be picked
        // up by another `fire_close_done` call.
        if self.is_closed() {
            self.fire_close_done();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
