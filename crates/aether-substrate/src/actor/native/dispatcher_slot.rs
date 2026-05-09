//! [`DispatcherSlot<A>`] — the [`Drainable`] adapter that wraps a
//! native actor for chassis worker-pool dispatch (issue 635 PR C).
//!
//! ## Relationship to `dispatch_loop_run`
//!
//! [`crate::actor::native::dispatch::dispatch_loop_run`] is the loop a
//! `Dedicated` actor runs on its own thread. It owns the actor, blocks
//! on `recv_blocking`, and runs the four-phase lifecycle
//! (main loop → drain after shutdown → on_close → registry close).
//!
//! `DispatcherSlot::run_cycle` is the *budget-bounded* version of the
//! same logic for the `Pooled` path. Each call to `run_cycle` does:
//!
//! 1. CAS `Ready → Running` on the [`SlotState`] (caller invariant:
//!    the slot was just popped from the ready queue).
//! 2. Drains envelopes via [`crate::NativeBinding::try_recv`] until
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
//!      post-shutdown drain + `on_close` hook + registry finalize
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

use aether_actor::local::ActorSlots;

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

impl std::ops::Deref for PooledSlots {
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
/// `Pooled` actor; held strongly by the chassis (so `on_close` and
/// registry finalize run when the cap shuts down) and weakly by the
/// pool's [`crate::scheduler::WakeHandle`] (so a wake after the cap
/// is gone silently no-ops).
pub(crate) struct DispatcherSlot<A>
where
    A: NativeActor + NativeDispatch,
{
    /// The slot's atomic state machine. Shared with the WakeHandle.
    pub(crate) state: Arc<SlotState>,
    /// The actor itself. The state machine guarantees only one worker
    /// is in `Running` at a time, so the `Mutex` is uncontested in
    /// production — used here over `UnsafeCell` only for the simpler
    /// safety story. `Option` so the `Closed` finalize path can take
    /// the box and run `on_close` on the consumed actor.
    actor: Mutex<Option<Box<A>>>,
    /// Per-actor binding (inbox + shutdown flag + reply machinery).
    binding: Arc<NativeBinding>,
    /// Per-actor `Local<T>` storage. Stamped into TLS for each
    /// envelope dispatch. Wrapped in [`PooledSlots`] for the `Sync`
    /// safety story — see that type's doc-comment.
    slots: PooledSlots,
    /// `FRAME_BARRIER` counter for this actor's mailbox. `None` for
    /// free-running caps; `Some` for frame-bound. Decremented after
    /// every successful dispatch.
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
    /// observes the flag and runs the `on_close` + registry finalize
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
        })
    }

    /// Per-envelope dispatch matching
    /// [`crate::actor::native::dispatch::dispatch_loop_run`]'s body.
    /// Wraps the dispatch call in `local::with_stamped` +
    /// `log_install::with_actor_dispatch` so tracing events carry the
    /// actor's mailbox id and the per-handler `LogBatch` ships at exit.
    fn dispatch_one(&self, actor: &mut Box<A>, env: crate::actor::native::Envelope) {
        aether_actor::local::with_stamped(&self.slots, || {
            crate::runtime::log_install::with_actor_dispatch(
                &*self.binding as &dyn crate::runtime::log_install::MailDispatch,
                || {
                    let mut ctx = NativeCtx::new(&self.binding, env.sender);
                    if actor
                        .__aether_dispatch_envelope(&mut ctx, env.kind, &env.payload)
                        .is_none()
                        && !actor.__aether_dispatch_fallback(&mut ctx, &env)
                    {
                        tracing::warn!(
                            target: "aether_substrate::dispatch",
                            actor = A::NAMESPACE,
                            kind = env.kind_name.as_str(),
                            "actor dispatch missed: kind not handled or decode failed"
                        );
                    }
                    aether_actor::log::drain_buffer();
                },
            );
        });
        if let Some(p) = &self.pending {
            p.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Phase 3 of the dispatch_loop_run lifecycle. Wraps `actor.on_close`
    /// in the same `with_stamped` + `with_actor_dispatch` envelope so a
    /// final tracing event from the close hook still routes to
    /// `LogCapability`.
    fn run_close_hook(&self, actor: &mut Box<A>) {
        aether_actor::local::with_stamped(&self.slots, || {
            crate::runtime::log_install::with_actor_dispatch(
                &*self.binding as &dyn crate::runtime::log_install::MailDispatch,
                || {
                    let mut close_ctx = NativeCtx::new(&self.binding, ReplyTo::NONE);
                    actor.on_close(&mut close_ctx);
                    aether_actor::log::drain_buffer();
                },
            );
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

        let mut actor_guard = self
            .actor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let actor = match actor_guard.as_mut() {
            Some(a) => a,
            None => {
                // Slot already finalized. Nothing to do.
                self.state.mark_idle();
                return CycleResult::Closed;
            }
        };

        let mut dispatched = 0u32;
        let mut shutdown_observed = false;
        let mut budget_hit = false;
        let mut inbox_empty = false;
        loop {
            if self.binding.should_shutdown() {
                shutdown_observed = true;
                break;
            }
            let env = match self.binding.try_recv() {
                Some(e) => e,
                None => {
                    inbox_empty = true;
                    break;
                }
            };
            self.dispatch_one(actor, env);
            dispatched += 1;
            if dispatched >= budget.max_mails || Instant::now() >= budget.deadline {
                budget_hit = true;
                break;
            }
        }

        if shutdown_observed {
            // Phase 2: drain residual inbox synchronously.
            while let Some(env) = self.binding.try_recv() {
                self.dispatch_one(actor, env);
            }
            // Phase 3: on_close hook.
            self.run_close_hook(actor);
            // Phase 4: registry close + monitor fan-out.
            self.finalize_registry();
            actor_guard.take();
            self.state.mark_idle();
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

    fn label(&self) -> &str {
        self.label
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
