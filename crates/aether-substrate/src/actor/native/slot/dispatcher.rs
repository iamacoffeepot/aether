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
//!    [`super::dispatch`].
//! 3. Returns one of:
//!    - [`CycleResult::Idle`] — inbox drained, post-empty recheck saw
//!      no race; worker drops the slot Arc.
//!    - [`CycleResult::Requeue`] — budget hit (state `Ready`) or
//!      post-empty recheck won the requeue CAS; worker re-pushes.
//!    - [`CycleResult::Closed`] — shutdown observed; the slot handed its
//!      actor to the one [`close`] sequence and is done forever.
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
//! [`Drainable::seize_and_run`] is the demux-direct counterpart to
//! [`Drainable::run_cycle`]: a [`crate::actor::native::burst::work::BurstWork`]
//! that has **seized** this slot (`Idle → Running`) hands it one
//! envelope to dispatch in place — skipping the inbox deposit +
//! `try_recv` repop the deposit-then-wake path paid. Both methods share
//! the same drain tail ([`DispatcherSlot::drain_after_seed`]).

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::actor::native::Envelope;
use crate::runtime::thread_name;
use aether_actor::local::ActorSlots;

use crate::actor::native::local;
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

use super::close::close;
use crate::actor::native::NativeActor;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::registry::ActorRegistry;
use crate::chassis::error::BootError;
use crate::mail::{MailId, MailboxId};
use crate::runtime::effect_chain::EffectChain;
use crate::scheduler::{
    BatchBudget, CLOCK_CHECK_STRIDE, CycleResult, Drainable, SeizeSeed, SlotState, cascade_note_mail, time_budget,
};

/// Worker-pool-side wrapper for a native actor. One instance per
/// `Pooled` actor; held strongly by the chassis (which signals, wakes, and
/// awaits its close at teardown) and weakly by the
/// pool's [`crate::scheduler::WakeHandle`] (so a wake after the cap
/// is gone silently no-ops). A spawned instanced actor's strong
/// reference is its spawner's entry, which the slot's own close cycle
/// gives up ([`Self::release_retained_slot`]): a closed instanced actor's
/// slot, with its rings and its binding, is freed once the worker that
/// ran the cycle returns.
pub struct DispatcherSlot<A>
where
    A: NativeActor,
{
    /// The slot's atomic state machine. Shared with the `WakeHandle`.
    pub(crate) state: Arc<SlotState>,
    /// The actor itself. This `Mutex` is the root mutual-exclusion +
    /// happens-before guarantor for a slot's dispatch: every drain runs
    /// under it (see [`Self::drain_after_seed`]), so two workers that
    /// reach the slot — e.g. a recheck-window dispatch racing a fresh
    /// `seize_and_run` — serialize here rather than relying on
    /// [`SlotState`] alone, which is the scheduling filter above it.
    /// `Option` so an exit can take the box and hand it to [`close`], which
    /// consumes it.
    actor: Mutex<Option<Box<A::State>>>,
    /// Per-actor binding (inbox + shutdown flag + reply machinery).
    binding: Arc<NativeBinding>,
    /// Per-actor `Local<T>` storage. Stamped into TLS for each
    /// envelope dispatch. Wrapped in [`PooledSlots`] for the `Sync`
    /// safety story — see that type's doc-comment.
    slots: PooledSlots,
    /// Chassis-level actor registry, which [`close`] ends this actor's name
    /// in.
    actor_registry: Arc<ActorRegistry>,
    /// This slot's mailbox id, which names the spawner entry the close
    /// cycle gives up.
    self_id: MailboxId,
    /// Static label for tracing / fairness logs. Today this is the
    /// actor's `NAMESPACE`.
    label: &'static str,
    /// Issue 714: one-shot completion sender installed by
    /// the chassis teardown walk (`Spawner::shutdown_instanced` for an
    /// instanced actor, the root's own shutdown for a composed one).
    /// Fired exactly once after the `Closed` branch of [`Self::run_cycle`]
    /// has run [`close`]. The walk waits on the matching receiver, so
    /// chassis teardown settles deterministically without a 2 ms polling
    /// loop. `Mutex<Option<_>>` so the slot can
    /// take + send without holding the lock across the actor mutex.
    close_done_tx: Mutex<Option<crossbeam_channel::Sender<()>>>,
}

impl<A> Drop for DispatcherSlot<A>
where
    A: NativeActor,
{
    /// The last resort for a slot freed with its actor still in it. Every
    /// exit the engine takes hands the actor to [`close`] on a pool worker
    /// first, and a slot that did so finds its ledger already emptied here.
    /// One freed around its close is left only by an exit the engine did not
    /// take: a worker that unwound mid-turn, or a pool that stopped with the
    /// slot's close cycle still queued.
    ///
    /// The close does not run here. Actor-authored code runs at the actor's
    /// execution home (ADR-0165), and this runs on whichever thread frees the
    /// slot. What does run is the silent settlement of the held replies and
    /// staged tasks (ADR-0243 §1, §9), so a `Held` or an unstarted task in
    /// the actor's state then drops silently with it and never panics as a
    /// lost reply.
    ///
    /// It writes no log line. A pool worker's thread-local deque can hold
    /// the last reference to a slot, so this can run inside a thread-local
    /// destructor at thread exit, where a log event reaches thread-local
    /// state that is already gone and aborts the process.
    fn drop(&mut self) {
        self.binding.settle_held_for_engine_teardown();
    }
}

impl<A> DispatcherSlot<A>
where
    A: NativeActor,
{
    /// Borrow this slot's [`SlotState`] — needed by callers building a
    /// [`crate::scheduler::WakeHandle`] over the slot.
    pub(crate) fn state(&self) -> &Arc<SlotState> {
        &self.state
    }

    pub(crate) fn new(
        actor: Box<A::State>,
        binding: Arc<NativeBinding>,
        slots: Box<ActorSlots>,
        actor_registry: Arc<ActorRegistry>,
        self_id: MailboxId,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(SlotState::new()),
            actor: Mutex::new(Some(actor)),
            binding,
            slots: PooledSlots(slots),
            actor_registry,
            self_id,
            label: A::NAMESPACE,
            close_done_tx: Mutex::new(None),
        })
    }

    /// Run the post-init wire hook while the activation job owns this slot.
    /// The slot is not routable or drainable yet.
    ///
    /// `chain` is the staging site's ADR-0168 §3 declaration, threaded here
    /// through the prepared activation because it is not otherwise in scope.
    /// A handler-staged birth declares [`EffectChain::Held`] with the chain
    /// its `spawn_child` ran on, so an effect the hook stages holds it and
    /// the staging caller's `Settled` covers the newborn's birth-completing
    /// work — the inline-child alias a `WasmTrampoline` publishes from `wire`
    /// is the motivating case. An embedder's post-seal `spawn_actor` reaches
    /// this same path from a thread holding no mail and declares so. Both
    /// pass the fresh wire root the birth opened for the hook's sends as
    /// `wire_root` (ADR-0244).
    ///
    /// The hook's result is the birth's (ADR-0247 rule 3): on `Err` the
    /// activation job cancels this slot through [`Self::cancel_activation`],
    /// which closes the actor, and the birth answers with the error.
    pub(crate) fn wire_activation(&self, chain: EffectChain, wire_root: Option<MailId>) -> Result<(), BootError> {
        let mut actor_guard = self.actor.lock().unwrap_or_else(PoisonError::into_inner);
        let actor = actor_guard.as_mut().expect("prepared activation owns an initialized actor");
        let wired = local::with_stamped(&self.slots, || {
            let mut ctx = NativeCtx::for_wire(&self.binding, chain, wire_root);
            A::wire(actor.as_mut(), &mut ctx)
        });
        drop(actor_guard);
        wired
    }

    /// Cancel a wired-but-not-live activation at the same execution home:
    /// the actor wired, so it closes. Its route still reads `Starting`, so
    /// [`close`] ends no name, and the activation hold still stands, so
    /// nothing `wire` or `unwire` sent leaves.
    pub(crate) fn cancel_activation(&self) {
        let mut actor_guard = self.actor.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(actor) = actor_guard.take() {
            self.close_here(actor);
        }
        drop(actor_guard);
        self.state.mark_idle();
    }

    /// Hand `actor` to the one [`close`] sequence, with this slot's inbox as
    /// its residual mail.
    fn close_here(&self, actor: Box<A::State>) {
        close::<A>(actor, &self.binding, &self.slots, &self.actor_registry, || self.binding.try_recv());
    }

    /// Issue 714: fire the installed one-shot completion sender if any.
    /// Called once from the `Closed` branch of [`Self::run_cycle`] after
    /// [`close`] has run. Take +
    /// `try_send`: bounded(1) guarantees the receiver only sees the
    /// first send; subsequent calls (idempotent — there should never be
    /// any) are no-ops. Done outside the actor mutex.
    fn fire_close_done(&self) {
        let tx = self.close_done_tx.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(tx) = tx {
            // Receiver may have hung up if the wait already timed out.
            // Either way, the channel goes away after this call.
            let _ = tx.try_send(());
        }
    }

    /// Issue #7402: give up the spawner's strong reference to this slot
    /// once its close cycle has run, so a closed actor's slot, rings and
    /// binding are freed rather than kept until chassis teardown. The
    /// worker running the cycle holds its own strong reference, so the
    /// slot outlives this call. A slot the spawner never retained (a
    /// composed singleton, a test binding with no spawner) releases
    /// nothing.
    fn release_retained_slot(&self) {
        if let Some(spawner) = self.binding.spawner() {
            spawner.release_closed_slot(self.self_id);
        }
    }

    /// Per-envelope dispatch — a one-line delegation to the shared
    /// [`dispatch_envelope`] free function, the single dispatch body both
    /// this pooled slot and the externally-pumped
    /// [`PumpedSlot`](super::pumped::PumpedSlot) run
    /// (ADR-0160 §1). Keeping the body in one place is what makes the two
    /// slots' dispatch semantics structurally identical rather than a copy
    /// that can drift.
    fn dispatch_one(&self, actor: &mut Box<A::State>, env: Envelope) {
        dispatch_envelope::<A>(actor, &self.binding, &self.slots, env);
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
                self.binding.mailer().record_finished(seed.mail_id, seed.root);
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
        // `t_enqueue` = the burst-pickup stamp the `BurstWork` demuxer took at
        // `run_cycle` entry, so `t_received − t_enqueue` is the real in-burst
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
            // The actor leaves the guard and is consumed by the one close:
            // residual drain, `unwire`, cost rows, held replies, registry
            // tail.
            if let Some(actor) = actor_guard.take() {
                self.close_here(actor);
            }
            // Drop the actor mutex before signalling so the waiter (the
            // chassis-teardown thread) wakes onto an unlocked slot.
            drop(actor_guard);
            self.state.mark_idle();
            // Issue #7402: the spawner gives the slot up here, after the
            // registry close queued the route drop and before close-done
            // fires, so a waiter that saw close-done knows the spawner
            // holds nothing of this actor.
            self.release_retained_slot();
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
        //
        // A shutdown signal races this cycle the same way and carries no
        // mail: the teardown walk sets the flag and then wakes, and a wake
        // that finds the slot `Running` does nothing. If the flag landed
        // after this cycle's last look at it, the cycle would park `Idle`
        // with its close never run and the walk waiting on it. So the flag
        // is read again after `mark_idle`, in the same total order as the
        // wake's CAS: either the wake saw `Idle` and queued the slot, or
        // this read sees the flag and requeues it.
        debug_assert!(inbox_empty);
        self.state.mark_idle();
        if let Some(env) = self.binding.try_recv() {
            self.dispatch_one(actor, env);
        } else if !self.binding.should_shutdown() {
            return CycleResult::Idle;
        }
        if self.state.try_self_requeue() {
            CycleResult::Requeue
        } else {
            CycleResult::Idle
        }
    }
}

impl<A> Drainable for DispatcherSlot<A>
where
    A: NativeActor,
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
    /// `signal_engine_teardown`, so the close settles held replies silently
    /// (ADR-0243 §1), and the next [`Self::run_cycle`] observes
    /// `should_shutdown` at the top of its drain loop and runs
    /// [`close`]. The chassis teardown walk calls this on every instanced
    /// slot and every composed root before firing a wake.
    fn signal_engine_teardown(&self) {
        self.binding.signal_engine_teardown();
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
        let prior = self.close_done_tx.lock().unwrap_or_else(PoisonError::into_inner).replace(tx);
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

/// The single per-envelope dispatch body both the pooled [`DispatcherSlot`]
/// and the externally-pumped
/// [`PumpedSlot`](super::pumped::PumpedSlot) run, so
/// there is exactly one dispatch semantics in the substrate (ADR-0160 §1).
/// Extracting it as a free function — rather than copying it into the
/// pumped slot — is what keeps the two homes structurally identical and
/// the jscpd duplicate-code gate quiet.
///
/// Wraps the dispatch in `local::with_stamped` so per-actor `Local<T>`
/// lookups (the ADR-0081 `ActorLogRing`, the per-actor cost cache) resolve
/// to `slots`; brackets the run with the ADR-0086 `Received` / `Finished`
/// trace hops; runs the framework-built-in arms
/// (`aether.{log,trace,cost}.tail`) then the typed / `#[fallback]`
/// dispatch; folds the handler's execution cost (iamacoffeepot/aether#1128);
/// and runs the ADR-0106 / ADR-0094 single-settlement tail — `record_finished`
/// + `discharge` unless a handler retained the inbound via `take_inbound`.
pub fn dispatch_envelope<A>(actor: &mut Box<A::State>, binding: &Arc<NativeBinding>, slots: &ActorSlots, env: Envelope)
where
    A: NativeActor,
{
    // iamacoffeepot/aether#1160: note this envelope against the worker's
    // local-drain cascade *before* running the handler, so a burst this
    // handler produces (scheduled at `ctx` drop below) is measured against
    // a cascade start that already covers this handler. With the time valve
    // on, the cascade's first mail anchors the start (one clock read per
    // cascade); with it off, this is a no-op. A pumped slot never runs the
    // time budget, so this is always a no-op there.
    cascade_note_mail(time_budget());
    // #1757: the single dispatched envelope moves into `ctx.inbound` below,
    // so read its `Copy` trace/settlement fields out first — the `Received`
    // / `Finished` / cost brackets and the settlement tail run off these
    // locals and never re-borrow the moved value.
    let mail_id = env.mail_id;
    let root = env.root;
    let kind = env.kind;
    let t_enqueue = env.t_enqueue;
    let enqueue_depth = env.enqueue_depth;
    let sender = env.sender;
    // Issue 734 / ADR-0088 §7: stamp the dispatching thread's name-hashed
    // `ThreadId` (a `Copy` u64) onto the `Received` event. Resolved once
    // per thread via a thread-local cache — no per-hop `str::to_owned`. On
    // the pooled path this is the worker's `aether-worker-N`; on the pumped
    // path it is the chassis driver thread that owns this slot.
    let thread_id = thread_name::current_thread_id();
    let inbound = local::with_stamped(slots, || {
        // ADR-0086 Phase 3: `Received` / `Finished` land in this
        // (recipient) actor's trace ring — only inside this `with_stamped`
        // is its `ActorSlots` stamped.
        let th = binding.mailer().trace_handle();
        // iamacoffeepot/aether#1128: capture the `Received` instant so the
        // cost fold below reuses the existing trace bracket — no new
        // timestamp on the hot path.
        let t_received = th.now_nanos();
        // A lineage-less envelope writes no ring entry: with no root, no
        // trace walk could reach it.
        let traced = mail_id.zip(root);
        if let Some((mail_id, root)) = traced {
            th.push_trace_ring(
                root,
                TraceEvent::Received {
                    mail_id,
                    t: t_received,
                    // iamacoffeepot/aether#1134: surface the deposit instant +
                    // scheduler backlog the producer stamped at `route_mail`,
                    // so the hop splits into send→enqueue + queue residence.
                    t_enqueue,
                    enqueue_depth,
                    thread_id,
                },
            );
        }
        // #1757 / ADR-0094: the dispatched envelope lives in exactly one
        // place — `ctx.inbound`. The dispatch arms read a disarmed *view*
        // (a `MailRef`-only clone whose obligation never fires), so the
        // single armed envelope settles exactly once: either the settlement
        // tail below discharges it, or a handler retained it via
        // `take_inbound`. #1774: the arms take `(kind, payload)` — the only
        // fields they read on the hot path — so the clone is an Arc-bump for
        // `InRing`, bytes-copy only for the rare `Owned`.
        let payload_view = env.payload.clone();
        // Issue 4158: typed by the actor being dispatched, so a handler that
        // named it in its ctx signature can parent a child under it. The
        // framework arms below are generic over the dispatched actor, and none
        // of them spawns.
        let mut ctx = NativeCtx::<'_, A, crate::Unchecked>::with_inbound(binding, sender, mail_id, root, env);
        let replied = ctx.in_reply_to();
        let payload = payload_view.bytes();
        // ADR-0081 / ADR-0086 / iamacoffeepot/aether#1128 framework-built-in
        // dispatch arms for `aether.log.tail` + `aether.trace.tail` +
        // `aether.cost.tail`. See the helper docs in `dispatch`.
        let typed_arm_ran = if super::dispatch::dispatch_log_tail_if_matching(&mut ctx, kind, payload)
            || super::dispatch::dispatch_trace_tail_if_matching(&mut ctx, kind, payload)
            || super::dispatch::dispatch_cost_tail_if_matching(binding, &mut ctx, kind, payload)
        {
            false
        } else {
            super::dispatch::typed_then_fallback_or_warn::<A>(actor, &mut ctx, kind, payload)
        };
        // #1757: reclaim the single envelope before the ctx (and its
        // handler-end flush) drops, so an armed inbound is never dropped
        // *inside* the ctx — that would trip the ADR-0094 guard. `None`
        // means a handler retained it via `take_inbound`.
        let inbound = ctx.take_raw_inbound();
        // Spike (log-stream-scale): hand this handler's new log lines to the
        // tap before the ctx drops, so a mailed slice rides the handler's own
        // flush. Closed, this is one relaxed load.
        #[cfg(not(feature = "spike-no-log-hook"))]
        super::dispatch::gather_log_lines(binding);
        // iamacoffeepot/aether#1150: flush before `Finished` so a child
        // `Sent` (stamped at flush-begin on `ctx` drop) precedes its
        // parent's `Finished`.
        drop(ctx);
        // ADR-0243 §7: a reply whose stored context still carries a parked
        // `Held` after its handler returned strands the debt, whether or not a
        // typed arm ran, so it fails fast naming the context kind.
        if let Some(request) = replied
            && let Some(context) = binding.parked_context(request)
        {
            panic!(
                "reply to request {} left its request context `{context}` untaken while it holds a `Held` \
                 (ADR-0243 §7): the handler must take_context and answer or stage the debt",
                request.0,
            );
        }
        let t_finished = th.now_nanos();
        if let Some((mail_id, root)) = traced {
            th.push_trace_ring(root, TraceEvent::Finished { mail_id, t: t_finished });
        }
        // iamacoffeepot/aether#1128: fold this handler's execution time into
        // its per-handler EWMA (lock-free through the per-actor cache;
        // framework / fallback kinds skipped). Measure-only. See
        // `dispatch::fold_handler_cost`.
        // iamacoffeepot/aether#4261: a typed arm that folds into no cell means
        // the actor never declared this kind, so its cost is unmeasured and
        // cost-aware recruitment silently degrades. Read off the fold's own
        // lookup — no second scan on the dispatch fast path.
        if !super::dispatch::fold_handler_cost(kind, t_received, t_finished) && typed_arm_ran {
            let _ = super::dispatch::warn_undeclared_handler_once(A::NAMESPACE, kind);
        }
        inbound
    });
    // #1757 / ADR-0080 §2 / ADR-0094: settle the single envelope exactly
    // once. `Some` is the normal path — `record_finished` beside
    // `discharge`, the canonical settle site every wasm component and
    // native actor drains through. `None` means a handler retained the
    // guard via `take_inbound`; its own un-fired `record_finished` rides
    // the retained `InboundMail` and closes the chain when that guard
    // drops, after its deferred reply.
    if let Some(env) = inbound {
        binding.mailer().record_finished(mail_id, root);
        env.discharge();
    }
}
