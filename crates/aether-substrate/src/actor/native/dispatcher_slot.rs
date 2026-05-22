//! [`DispatcherSlot<A>`] — the [`Drainable`] adapter that wraps a
//! native actor for chassis worker-pool dispatch (issue 635 PR C).
//!
//! ## Relationship to `dispatch_loop_run`
//!
//! [`crate::actor::native::dispatch::dispatch_loop_run`] is the loop a
//! `Dedicated` actor runs on its own thread. It owns the actor, blocks
//! on `recv_blocking`, and runs the four-phase lifecycle
//! (main loop → drain after shutdown → unwire → registry close).
//!
//! `DispatcherSlot::run_cycle` is the *budget-bounded* version of the
//! same logic for the `Pooled` path. Each call to `run_cycle` does:
//!
//! 1. CAS `Ready → Running` on the [`SlotState`] (caller invariant:
//!    the slot was just popped from the ready queue).
//! 2. Drains envelopes via [`NativeBinding::try_recv`] until
//!    inbox is empty, the budget is exhausted, or shutdown fires.
//!    Per-envelope wrapping is `local::with_stamped(slots, ...)` +
//!    `log_install::with_actor_dispatch(binding, ...)` — same as
//!    `dispatch_loop_run`'s body so traces / `Local<T>` lookups behave
//!    identically.
//! 3. Returns one of:
//!    - [`CycleResult::Idle`] — inbox drained, post-empty recheck saw
//!      no race; worker drops the slot Arc.
//!    - [`CycleResult::Requeue`] — budget hit (state `Ready`) or
//!      post-empty recheck won the requeue CAS; worker re-pushes.
//!    - [`CycleResult::Closed`] — shutdown observed; the slot ran the
//!      post-shutdown drain + `unwire` hook + registry finalize
//!      sequence and is done forever.
//!
//! ## Today (PR C)
//!
//! Every actor in the workspace ships `SCHEDULING = Dedicated`, so
//! this slot is constructed by the `Pooled` branch of
//! `make_native_actor_boot` / `Spawner::spawn_actor` but never
//! actually reached at runtime. The branch + slot impl are shaped so
//! Phase 2 (PR D) can flip a single cap to `Pooled` and have the
//! pool drive it.

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::actor::native::Envelope;
use aether_actor::local;
use aether_actor::local::ActorSlots;
use std::ops::Deref;
use std::sync::PoisonError;
use std::thread;

/// `ActorSlots` uses `RefCell` internally because the dedicated-thread
/// dispatcher path only ever reaches it from one OS thread. Worker-pool
/// dispatch can have *different* worker threads hit the same slot
/// across cycles — but the [`SlotState`] machine guarantees only one
/// worker is in `Running` at a time. That serialization makes the
/// `RefCell` accesses sound; this wrapper is the safety story.
#[repr(transparent)]
struct PooledSlots(Box<ActorSlots>);

// SAFETY: see the doc-comment on `PooledSlots`. The SlotState
// machine's `Idle → Ready → Running → Idle` invariant ensures at most
// one worker thread holds an active reference to the inner
// `ActorSlots` at any time, so the inner `RefCell` is effectively
// single-threaded across the Pooled dispatch path.
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
use crate::mail::{KindId, Mail, MailboxId, ReplyTo};
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState};

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
    /// The actor itself. The state machine guarantees only one worker
    /// is in `Running` at a time, so the `Mutex` is uncontested in
    /// production — used here over `UnsafeCell` only for the simpler
    /// safety story. `Option` so the `Closed` finalize path can take
    /// the box and run `unwire` on the consumed actor.
    actor: Mutex<Option<Box<A>>>,
    /// Per-actor binding (inbox + shutdown flag + reply machinery).
    binding: Arc<NativeBinding>,
    /// Per-actor `Local<T>` storage. Stamped into TLS for each
    /// envelope dispatch. Wrapped in [`PooledSlots`] for the `Sync`
    /// safety story — see that type's doc-comment.
    slots: PooledSlots,
    /// Per-actor inbox-pending counter, decremented after every
    /// successful dispatch. `None` for singleton caps (ADR-0082 retired
    /// the frame-bound drain that was the singleton consumer); `Some`
    /// for instanced actors, where `Spawner::shutdown_instanced` reads
    /// it to coordinate teardown (issue 685).
    pending: Option<Arc<AtomicU64>>,
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
        pending: Option<Arc<AtomicU64>>,
        actor_registry: Arc<ActorRegistry>,
        mailer: Arc<Mailer>,
        self_id: MailboxId,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(SlotState::new()),
            actor: Mutex::new(Some(actor)),
            binding,
            slots: PooledSlots(slots),
            pending,
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

    /// Per-envelope dispatch matching
    /// [`crate::actor::native::dispatch::dispatch_loop_run`]'s body.
    /// Wraps the dispatch call in `local::with_stamped` so per-actor
    /// `Local<T>` lookups (including the ADR-0081 `ActorLogRing`)
    /// resolve to this actor's slots. ADR-0081 retired the prior
    /// `log_install::with_actor_dispatch` wrap + per-handler flush hop.
    // `env` is taken by value because dispatch may move fields out
    // (reply_to, payload) into the actor; the helper here doesn't
    // happen to, but the surface mirrors the owning dispatch loop.
    #[allow(clippy::needless_pass_by_value)]
    fn dispatch_one(&self, actor: &mut Box<A>, env: Envelope) {
        let inbound_mail_id = env.mail_id;
        // Issue 734: capture the OS thread name at the dispatcher's
        // receive hook so the trace renderer can stamp each event
        // with a per-thread tid + thread_name M event. With the
        // `Pooled` default scheduler (issue 635) this is the worker's
        // `aether-worker-N` name (shared across actors); `Thread`-
        // scheduled actors land on a per-actor name.
        let thread_name = thread::current().name().map(str::to_owned);
        self.binding
            .mailer()
            .record_received(inbound_mail_id, env.root, thread_name);
        local::with_stamped(&self.slots, || {
            let mut ctx = NativeCtx::new(&self.binding, env.sender, env.mail_id, env.root);
            // ADR-0081 framework-built-in dispatch arm for
            // `aether.log.tail`. See `dispatch::dispatch_loop_run`.
            if !super::dispatch::dispatch_log_tail_if_matching(&mut ctx, &env) {
                super::dispatch::typed_then_fallback_or_warn::<A>(actor, &mut ctx, &env);
            }
        });
        self.binding
            .mailer()
            .record_finished(inbound_mail_id, env.root);
        if let Some(p) = &self.pending {
            p.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Phase 3 of the `dispatch_loop_run` lifecycle. Wraps `actor.unwire`
    /// in `with_stamped` so any final tracing or `Local<T>` access from
    /// the close hook resolves to this actor's slots.
    fn run_close_hook(&self, actor: &mut Box<A>) {
        local::with_stamped(&self.slots, || {
            let mut close_ctx = NativeCtx::new(
                &self.binding,
                ReplyTo::NONE,
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

        let mut actor_guard = self.actor.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(actor) = actor_guard.as_mut() else {
            // Slot already finalized. Nothing to do.
            drop(actor_guard);
            self.state.mark_idle();
            // Issue 714: a wait that came in after the close cycle
            // already ran needs the signal too.
            self.fire_close_done();
            return CycleResult::Closed;
        };

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
            if dispatched.is_multiple_of(crate::scheduler::CLOCK_CHECK_STRIDE) {
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
