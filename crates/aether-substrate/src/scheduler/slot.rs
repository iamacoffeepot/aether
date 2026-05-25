//! Per-slot state machine + the [`Drainable`] trait the chassis-side
//! dispatcher slot implements.
//!
//! The state machine orchestrates the "many actors, few workers"
//! invariant: each slot is in one of three states, and the
//! `Idle → Ready` CAS is what decides whether a sender pushes the
//! slot onto the ready queue.
//!
//! ```text
//!         +------ send arrives, CAS Idle→Ready ----+
//!         |                                        |
//!         v                                        |
//!     +-------+    pop from ready queue       +---------+    inbox empty + recheck     +-------+
//!     | Idle  | ----------------------------> | Running | -------------------------->  | Idle  |
//!     +-------+                               +---------+                              +-------+
//!         ^                                       |
//!         |              budget hit               |
//!         |              CAS Running→Ready        |
//!         |              re-push                  |
//!         +---------------------------------------+
//! ```
//!
//! The "post-empty recheck" idiom closes the classic send-vs-drain
//! race: a sender that observes `Running` skips the wake (the worker
//! is still draining and will see the new mail). If the worker drains
//! to empty and transitions to `Idle` *after* the sender pushed but
//! *before* the worker re-checks the inbox, the slot looks idle but
//! has unprocessed mail. The recheck catches that case and re-runs the
//! `Idle → Ready` CAS to requeue.

use std::any::Any;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use crossbeam_channel::Sender;
use crossbeam_deque::Injector;

use super::spin_park::SpinPark;
use super::worker_deque;
use crate::actor::native::Envelope;

/// The one envelope a [`Drainable::seize_and_run`] caller hands the
/// just-seized slot to dispatch in place (ADR-0087 §4,
/// iamacoffeepot/aether#1135). Alias for the actor-layer
/// [`Envelope`] the `BlobWork` demuxer
/// builds from the blob's [`Mail`](crate::mail::Mail), with
/// `enqueue_depth = 0` and (iamacoffeepot/aether#1150) `t_enqueue` set to
/// the blob-pickup instant, so the recipient's `Received` reads a real
/// `t_received − t_enqueue` drain rather than the pre-#1150 ≈ 0.
pub type SeizeSeed = Envelope;

/// Default per-cycle envelope cap. Once a worker has dispatched this
/// many envelopes from a single slot, it yields to other ready slots
/// to keep one chatty actor from monopolising the pool. Tunable in
/// Phase 2 once measurement points at a better value.
pub const BATCH_MAX_MAILS: u32 = 64;

/// Default per-cycle wallclock budget in microseconds. The worker
/// checks this between envelopes (so a single slow handler can exceed
/// it; the cap is a fairness backstop, not a hard deadline). Tunable
/// in Phase 2.
pub const BATCH_MAX_USEC: u64 = 200;

/// Check the wallclock deadline only every Nth dispatch (iamacoffeepot/aether#1067).
/// A warm single/few-mail cycle drains to empty before reaching the
/// stride, so it never reads the clock; the time cap still engages once
/// a slot is genuinely batching. The count cap (`BATCH_MAX_MAILS`) is
/// the hard backstop and is checked every dispatch (no clock).
pub const CLOCK_CHECK_STRIDE: u32 = 8;

/// `Idle`: inbox empty, slot is not in the ready queue.
const STATE_IDLE: u8 = 0;
/// `Ready`: inbox non-empty, slot is in the ready queue and waiting
/// for a worker to pop it.
const STATE_READY: u8 = 1;
/// `Running`: a worker has popped the slot and is currently draining.
const STATE_RUNNING: u8 = 2;

/// Atomic state machine for a single dispatcher slot. Held in an
/// `Arc<SlotState>` so the inbox-sender side ([`WakeHandle`]) and the
/// worker side (the [`Drainable`]) operate on the same atomic.
#[derive(Debug)]
pub struct SlotState {
    state: AtomicU8,
}

impl SlotState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_IDLE),
        }
    }

    /// Sender-side wake: attempts the `Idle → Ready` transition.
    /// Returns `true` if the caller is the one transition that won
    /// (and is therefore responsible for pushing the slot to the
    /// ready queue). `false` means the slot was already in flight
    /// (`Ready` or `Running`); the existing scheduling will pick up
    /// the newly-pushed envelope.
    pub fn try_wake(&self) -> bool {
        self.state
            .compare_exchange(STATE_IDLE, STATE_READY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Worker-side: claim a `Ready` slot for a drain cycle. Returns
    /// `true` on the winning transition. Should always succeed under
    /// the documented invariants — only the popper from the ready
    /// queue calls this, and a slot is only in the queue while
    /// `Ready`.
    pub fn enter_running(&self) -> bool {
        self.state
            .compare_exchange(
                STATE_READY,
                STATE_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Demux-side: claim a `free` slot for an *in-place* dispatch
    /// (ADR-0087 §4, iamacoffeepot/aether#1135). CAS `Idle → Running`,
    /// returning `true` on the winning transition. Distinct from
    /// [`Self::enter_running`] (`Ready → Running`): a blob demuxer holds
    /// the recipient's mail in hand and wants to run it *without* an
    /// inbox round-trip, so it seizes a slot that no sender has woken
    /// yet (state `Idle`). A `false` means the slot is already in flight
    /// (`Ready` — a sender's `try_wake` won; or `Running` — a worker is
    /// draining): the demuxer falls back to depositing the mail through
    /// `route_mail`, and the holder/woken cycle drains it (per-recipient
    /// FIFO preserved by the inbox's own ordering).
    pub fn seize(&self) -> bool {
        self.state
            .compare_exchange(
                STATE_IDLE,
                STATE_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Worker-side: leave a drained-to-empty slot. The transition
    /// `Running → Idle` is unconditional (we hold the slot exclusively
    /// while `Running`).
    pub fn mark_idle(&self) {
        self.state.store(STATE_IDLE, Ordering::Release);
    }

    /// Worker-side: budget hit — leave the slot in `Ready` so the
    /// caller can re-push it to the ready queue without going through
    /// `Idle`.
    pub fn mark_ready(&self) {
        self.state.store(STATE_READY, Ordering::Release);
    }

    /// Post-empty recheck CAS: after `mark_idle` we re-check the
    /// inbox; if non-empty, race with any sender's `try_wake` to
    /// claim the requeue slot. `true` means we won and the caller
    /// re-pushes the slot; `false` means a concurrent sender beat us
    /// to it (and they re-pushed).
    pub fn try_self_requeue(&self) -> bool {
        self.state
            .compare_exchange(STATE_IDLE, STATE_READY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Snapshot the current state. For tests + assertions; production
    /// scheduling never reads the state directly outside the methods
    /// above.
    pub fn current(&self) -> SlotStateLabel {
        match self.state.load(Ordering::Acquire) {
            STATE_IDLE => SlotStateLabel::Idle,
            STATE_READY => SlotStateLabel::Ready,
            STATE_RUNNING => SlotStateLabel::Running,
            _ => unreachable!("SlotState only stores 0..=2"),
        }
    }
}

impl Default for SlotState {
    fn default() -> Self {
        Self::new()
    }
}

/// Human-readable form of a [`SlotState`] snapshot. Production paths
/// don't branch on this; tests + tracing logs use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStateLabel {
    Idle,
    Ready,
    Running,
}

/// Per-cycle drain budget. The worker constructs one budget per slot
/// visit and hands it to [`Drainable::run_cycle`]; the slot consumes
/// against it as it dispatches envelopes.
#[derive(Debug, Clone, Copy)]
pub struct BatchBudget {
    /// Max number of envelopes to dispatch this cycle.
    pub max_mails: u32,
    /// Max wallclock duration for the drain cycle. The slot computes a
    /// deadline lazily from this (only once it is batching past
    /// [`CLOCK_CHECK_STRIDE`]), so constructing a budget no longer reads
    /// the clock and a warm single-mail cycle never does either
    /// (iamacoffeepot/aether#1067).
    pub max_dur: Duration,
}

impl BatchBudget {
    /// Default budget per the const knobs at the top of this module.
    #[must_use]
    pub fn standard() -> Self {
        Self::custom(BATCH_MAX_MAILS, Duration::from_micros(BATCH_MAX_USEC))
    }

    /// Custom budget. Mostly for tests.
    #[must_use]
    pub fn custom(max_mails: u32, max_duration: Duration) -> Self {
        Self {
            max_mails,
            max_dur: max_duration,
        }
    }
}

/// Outcome the slot's drain body returns to the worker. The worker
/// uses this to decide whether to drop the slot or re-push it to the
/// ready queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Inbox drained to empty. The slot's [`Drainable::run_cycle`]
    /// then runs the post-empty recheck before returning the cycle
    /// result.
    Empty,
    /// Budget hit (mail count or wallclock). The slot still has work
    /// — the worker re-pushes the slot.
    Yielded,
    /// Sender side disconnected (chassis shutdown / actor dropped).
    /// The worker should drop the slot.
    Closed,
}

/// What the worker should do with the slot after [`Drainable::run_cycle`]
/// returns. Decided by the slot itself (it owns the state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleResult {
    /// Slot is parked in `Idle`. Worker drops the `Arc<dyn Drainable>`
    /// it popped — the slot stays alive via the chassis-held `Arc`,
    /// but is no longer in flight on the ready queue.
    Idle,
    /// Slot is in `Ready`. Worker re-pushes it to the ready queue so
    /// it gets another scheduling round (either yielded mid-drain or
    /// post-empty recheck found new work).
    Requeue,
    /// Sender side disconnected. Worker drops; slot will be dropped
    /// from the chassis registry on the next drop-on-shutdown sweep.
    Closed,
}

/// Trait the chassis-side dispatcher slot implements. PR C will
/// implement this for `DispatcherSlot<A>` (one per `Pooled` actor).
/// PR B exercises it via an in-module `CounterSlot` test fixture.
///
/// Implementors own:
/// - The per-slot [`SlotState`] (so this trait can poll it).
/// - The actor's inbox (so [`run_cycle`](Self::run_cycle) can drain).
/// - Whatever the actor's handler invocation needs (a `Box<A>` plus
///   the per-envelope wrapping that the crate-internal
///   `dispatch_loop_run` does in `crate::actor::native::dispatch` —
///   `local::with_stamped`, `log_install::with_actor_dispatch`, etc).
pub trait Drainable: Send + Sync + 'static {
    /// One drain cycle. Sequence:
    /// 1. CAS `Ready → Running` (caller holds the invariant: this
    ///    slot was just popped from the ready queue, so its state is
    ///    `Ready`).
    /// 2. Drain envelopes against `budget` until `Empty` / `Yielded`
    ///    / `Closed`.
    /// 3. Run the post-empty recheck if we drained empty.
    /// 4. Return the [`CycleResult`] telling the worker what to do.
    fn run_cycle(&self, budget: BatchBudget) -> CycleResult;

    /// In-place demux dispatch (ADR-0087 §4, iamacoffeepot/aether#1135).
    /// The caller has just **seized** this slot
    /// ([`SlotState::seize`] / [`SeizeHandle::try_seize`]) — state is
    /// `Running` — and holds one envelope for it (`seed`). Run the full
    /// per-envelope wrapper on `seed` *without* an inbox deposit +
    /// `try_recv` repop, then drain the rest of the inbox under `budget`
    /// and run the post-empty recheck — i.e. the same drain tail as
    /// [`Self::run_cycle`]. Returns the [`CycleResult`] telling the
    /// demuxer what to do with the slot (`Requeue` → re-schedule; `Idle`
    /// / `Closed` → drop).
    ///
    /// Default: deposit-only / unreachable. Mock fixtures (and the
    /// `BlobWork` blob itself, which has no actor of its own) never get
    /// seized, so the default just parks the slot back to `Idle` and
    /// returns. A real `DispatcherSlot` overrides it.
    fn seize_and_run(&self, _seed: SeizeSeed, _budget: BatchBudget) -> CycleResult {
        CycleResult::Idle
    }

    /// Debug label — used in tracing events when the worker logs slot
    /// activity. Default `"<unnamed>"`. Implementors override with
    /// something stable (e.g. the actor's namespace).
    fn label(&self) -> &'static str {
        "<unnamed>"
    }

    /// Issue 685: chassis-teardown signal for `Pooled` instanced
    /// actors. The chassis calls this on every spawned slot before the
    /// pool drops; a real slot forwards to its
    /// [`crate::actor::native::NativeBinding::signal_shutdown`] so the
    /// next [`Self::run_cycle`] observes `should_shutdown` and runs
    /// the close path (drain residual → `unwire` → registry close +
    /// monitor fan-out). Default no-op so mock fixtures don't have to
    /// care.
    fn signal_shutdown(&self) {}

    /// Issue 685: chassis-teardown wait predicate. Returns `true` once
    /// the slot has finished its `CycleResult::Closed` cycle and the
    /// actor's `unwire` + registry close have run. Default `true`
    /// (mock fixtures are trivially "closed" — they don't have a
    /// real lifecycle).
    fn is_closed(&self) -> bool {
        true
    }

    /// Issue 714: install a one-shot completion sender the slot fires
    /// when its [`CycleResult::Closed`] cycle finishes — i.e. after
    /// `unwire` + registry close ran and the actor box was taken out.
    /// The crate-internal `Spawner::shutdown_instanced` (in
    /// `crate::actor::native::spawn`) uses this to settle on each
    /// spawned slot via `recv_timeout` instead of polling
    /// [`Self::is_closed`] in a 2 ms loop, which flaked under nextest
    /// contention.
    ///
    /// Default no-op: mock fixtures don't have a real close cycle so
    /// they never need to signal. Idempotent — the slot only fires the
    /// first installed sender once; a re-install after close is a no-op.
    fn set_close_done_tx(&self, _tx: Sender<()>) {}

    /// Upcast helper for downcasting in tests. Production code doesn't
    /// reach for this.
    fn as_any(&self) -> &dyn Any;
}

/// The destination a [`WakeHandle`] routes a *spilled* slot to: the
/// pool's shared [`Injector`] plus the [`SpinPark`] coordinator it
/// notifies after pushing (ADR-0087 Phase 3a — the shared
/// `crossbeam_channel` ready queue retired). Bundled so the "where woken
/// work goes + how we wake a worker" pair travels as one value through
/// the chassis wiring. Cloned per registered slot. (The affinity path —
/// pushing to the *current worker's own* deque — goes through
/// the `worker_deque` thread-local, not this sink.)
#[derive(Clone)]
pub struct WakeSink {
    injector: Arc<Injector<Arc<dyn Drainable>>>,
    spin: Arc<SpinPark>,
    /// Pool worker count — the hard cap on how many blob clones a recruit
    /// injects (iamacoffeepot/aether#1147). Recruiting more copies than
    /// workers cannot add parallelism (no more than `workers` can drain
    /// concurrently); the excess just churns the injector. See
    /// [`Self::recruit`].
    workers: usize,
}

impl WakeSink {
    /// Bundle the pool's shared injector, spin/park coordinator, and worker
    /// count. The chassis builds one from [`super::PoolHandle::wake_sink`].
    #[must_use]
    pub fn new(
        injector: Arc<Injector<Arc<dyn Drainable>>>,
        spin: Arc<SpinPark>,
        workers: usize,
    ) -> Self {
        Self {
            injector,
            spin,
            workers,
        }
    }

    /// Schedule a runnable `slot`: push to the current worker's own
    /// deque when this runs on a pool worker under the local bound (the
    /// affinity warm path — no notify, the same worker drains it LIFO),
    /// else spill to the shared injector and notify the coordinator
    /// (route-to-spinner / unpark-one). This is the non-demux wake
    /// destination, shared by [`WakeHandle::wake`], the producer-side
    /// blob push, and an inline recipient that yielded mid-drain
    /// (ADR-0087 Phase 3b). The injector push is infallible; shutdown is
    /// observed through the coordinator's flag.
    pub(crate) fn schedule(&self, slot: Arc<dyn Drainable>) {
        if let Err(slot) = worker_deque::try_push_local(slot, worker_deque::sticky_cap()) {
            self.injector.push(slot);
            self.spin.notify();
        }
    }

    /// Recruit `count` workers to a shared cooperative blob
    /// (iamacoffeepot/aether#1137): push `count` clones of the same
    /// `Drainable` onto the shared injector, then wake up to `count` parked
    /// siblings to race its cursor. This is the broadcast-recruit the
    /// own-deque [`Self::schedule`] cannot do (that path keeps work local
    /// with no notify). Over-recruiting is harmless: a worker that pops a
    /// copy after the cursor is drained finds no group to claim and drops
    /// the clone. `count == 0` is a no-op.
    ///
    /// The wake goes through [`SpinPark::wake_workers`], **not** a per-clone
    /// [`SpinPark::notify`]: `notify` routes-to-spinner (skips the unpark
    /// when any worker is spinning), and a single spinner cannot drain
    /// `count` independent clones — so per-clone notifies collapse the
    /// recruitment to whoever was already spinning, leaving parked siblings
    /// idle (iamacoffeepot/aether#1143). One batch wake unparks the parked
    /// siblings directly; spinners still scan the clones as a bonus.
    ///
    /// `count` is capped at `workers - 1` (iamacoffeepot/aether#1147): the
    /// producer drains its own [`Self::schedule`] copy, so at most that many
    /// *other* workers can take clones. A wide fan-out asks for up to
    /// `recruit_cap` (default 32); injecting more clones than workers cannot
    /// add parallelism (only `workers` can drain at once) and just churns the
    /// injector — each excess clone is a push + steal + `Arc` clone/drop and
    /// a no-op `run_cycle` on an already-drained cursor.
    pub(crate) fn recruit(&self, slot: &Arc<dyn Drainable>, count: usize) {
        let count = count.min(self.workers.saturating_sub(1));
        for _ in 0..count {
            self.injector.push(Arc::clone(slot));
        }
        self.spin.wake_workers(count);
    }
}

/// Sender-side wake hook the chassis hands to the inbox sender path
/// (PR C wires this into `MailboxSender`). Holds a [`Weak<dyn
/// Drainable>`] to the slot — the chassis registry owns the strong
/// reference, so the wake handle going stale just means the slot was
/// already dropped and we silently no-op.
#[derive(Clone)]
pub struct WakeHandle {
    state: Arc<SlotState>,
    slot: Weak<dyn Drainable>,
    sink: WakeSink,
}

impl WakeHandle {
    /// Construct a wake handle. The chassis registry calls this when
    /// it wires a new dispatcher slot.
    pub fn new(state: Arc<SlotState>, slot: Weak<dyn Drainable>, sink: WakeSink) -> Self {
        Self { state, slot, sink }
    }

    /// Wake the slot if it isn't already in flight. Called from the
    /// inbox sender path *after* the envelope is pushed onto the
    /// inbox channel.
    ///
    /// Returns `true` if this call won the `Idle → Ready` CAS (and
    /// thus pushed the slot to the ready queue), `false` if the slot
    /// was already `Ready`/`Running` or has been dropped.
    #[must_use]
    pub fn wake(&self) -> bool {
        if !self.state.try_wake() {
            return false;
        }
        let Some(slot) = self.slot.upgrade() else {
            // Slot was dropped between the CAS and our upgrade. The
            // ready queue won't see this slot; nothing to do. We
            // already flipped state to Ready, but since the slot is
            // gone the state never matters.
            return false;
        };
        // Today's affinity / spill path (iamacoffeepot/aether#1059
        // own-deque, #1064 route-to-spinner): own deque under the local
        // bound keeps a chain warm with no notify; otherwise spill to the
        // injector + notify a spinner (or unpark one). See
        // [`WakeSink::schedule`]. The Phase 3b blob-demux deposit+collect
        // arm retired in iamacoffeepot/aether#1135 — `BlobWork` now
        // seizes free recipients (`Idle → Running`) and dispatches in
        // place rather than depositing through `route_mail` and
        // collecting the woken slot here.
        self.sink.schedule(slot);
        true
    }

    /// Borrow the slot state. Tests reach for this; production code
    /// goes through `wake`.
    #[must_use]
    pub fn state(&self) -> &Arc<SlotState> {
        &self.state
    }
}

/// Demux-side handle to a recipient's dispatcher slot (ADR-0087 §4,
/// iamacoffeepot/aether#1135). Surfaced on the registry's
/// [`MailboxEntry::Inbox`](crate::mail::registry::MailboxEntry) entry so
/// a `BlobWork` demuxing a fan-out can resolve recipient → slot up front
/// and dispatch its mail *in place* — seizing the slot (`Idle → Running`)
/// and running the full per-envelope wrapper rather than depositing the
/// mail on the inbox mpsc and bouncing it back out through a `try_recv`
/// repop.
///
/// Holds the same pair the [`WakeHandle`] does — the slot's
/// [`SlotState`] (so the demuxer can drive the `Idle → Running` seize
/// CAS) and a [`Weak<dyn Drainable>`] (so a seize after the slot dropped
/// silently no-ops). The chassis registry owns the strong slot ref; this
/// handle going stale just means the actor was torn down. Only `Pooled`
/// actors expose one — closure / `Inline` handlers have no slot to seize,
/// so their entry carries `None` and the demuxer deposits as usual.
#[derive(Clone)]
pub struct SeizeHandle {
    state: Arc<SlotState>,
    slot: Weak<dyn Drainable>,
}

impl SeizeHandle {
    /// Construct a seize handle over a `Pooled` slot. The Pooled-branch
    /// wiring in `chassis/builder.rs` + `actor/native/spawn.rs` builds
    /// one once the slot exists and installs it into the registry entry.
    #[must_use]
    pub fn new(state: Arc<SlotState>, slot: Weak<dyn Drainable>) -> Self {
        Self { state, slot }
    }

    /// Try to claim the recipient slot for an in-place seed dispatch.
    /// Wins the `Idle → Running` CAS and upgrades the weak slot ref →
    /// `Some(slot)` (the demuxer then runs [`Drainable::seize_and_run`]
    /// on it and is responsible for the resulting [`CycleResult`]). A
    /// `None` means the slot is busy (`Ready`/`Running` — lost the CAS)
    /// or already dropped: the caller deposits the mail through
    /// `route_mail` instead.
    ///
    /// On a lost CAS the state is untouched (`compare_exchange` only
    /// flips on success), so a concurrent holder keeps its claim. On a
    /// won CAS with a dropped slot we revert the state back to `Idle` so
    /// it doesn't strand in `Running` — there is nothing to run.
    #[must_use]
    pub fn try_seize(&self) -> Option<Arc<dyn Drainable>> {
        if !self.state.seize() {
            return None;
        }
        let upgraded = self.slot.upgrade();
        if upgraded.is_none() {
            // Slot dropped between the CAS and the upgrade. We hold a
            // `Running` claim on a corpse — revert to `Idle` so a later
            // wake (against a re-registered slot under the same id) isn't
            // blocked.
            self.state.mark_idle();
        }
        upgraded
    }

    /// Borrow the slot state. Tests reach for this to assert the
    /// post-cycle label; production code goes through [`Self::try_seize`].
    #[must_use]
    pub fn state(&self) -> &Arc<SlotState> {
        &self.state
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture inbox `Mutex` lock and decode panic on failure is the assertion"
)]
pub mod tests {
    use super::*;
    use std::any::Any;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use std::time::Duration;

    use crossbeam_deque::Steal;
    use std::collections::VecDeque;
    use std::hint;
    use std::thread;

    /// Test fixture: a slot with a `Vec<u32>` inbox and a counter
    /// incremented per dispatch. Exercises the [`Drainable`] surface
    /// without dragging in the real chassis machinery.
    pub struct CounterSlot {
        pub state: Arc<SlotState>,
        pub inbox: Mutex<VecDeque<u32>>,
        pub closed: AtomicBool,
        pub dispatched: AtomicU32,
        /// If `Some(n)`, the n-th dispatch (1-indexed) panics. Used by
        /// the panic-isolation test in the `pool` module.
        pub panic_at: Option<u32>,
        /// Per-envelope work duration. Used by the time-budget test.
        pub work_per_env: Duration,
        pub label: &'static str,
        /// Optional downstream relay: on each dispatch, forward the env to
        /// this slot's inbox and wake it. The wake runs on the pool
        /// worker, so a chain of these exercises the worker-local stash
        /// path (iamacoffeepot/aether#1059).
        pub forward: Mutex<Option<(Arc<Self>, WakeHandle)>>,
    }

    impl CounterSlot {
        pub fn new(label: &'static str) -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(SlotState::new()),
                inbox: Mutex::new(VecDeque::new()),
                closed: AtomicBool::new(false),
                dispatched: AtomicU32::new(0),
                panic_at: None,
                work_per_env: Duration::ZERO,
                label,
                forward: Mutex::new(None),
            })
        }

        pub fn with_panic_at(mut self: Arc<Self>, n: u32) -> Arc<Self> {
            // Safe: caller hasn't shared the Arc with anyone else yet.
            Arc::get_mut(&mut self).expect("uniquely owned").panic_at = Some(n);
            self
        }

        pub fn push(&self, env: u32) {
            self.inbox.lock().unwrap().push_back(env);
        }

        pub fn close_inbox(&self) {
            self.closed.store(true, Ordering::Release);
        }

        pub fn dispatched(&self) -> u32 {
            self.dispatched.load(Ordering::Acquire)
        }

        fn try_recv(&self) -> Option<u32> {
            self.inbox.lock().unwrap().pop_front()
        }

        fn inbox_is_empty(&self) -> bool {
            self.inbox.lock().unwrap().is_empty()
        }

        fn drain_one(&self, env: u32) {
            let n = self.dispatched.fetch_add(1, Ordering::AcqRel) + 1;
            assert_ne!(
                Some(n),
                self.panic_at,
                "CounterSlot panic at envelope #{n} (test-induced)"
            );
            hint::black_box(env);
            if !self.work_per_env.is_zero() {
                thread::sleep(self.work_per_env);
            }
            // Relay: forward to the downstream slot and wake it. On a pool
            // worker this wake stashes the downstream in the worker-local
            // cell (the path under test).
            if let Some((down, wake)) = &*self.forward.lock().unwrap() {
                down.push(env);
                let _ = wake.wake();
            }
        }
    }

    impl Drainable for CounterSlot {
        fn run_cycle(&self, budget: BatchBudget) -> CycleResult {
            // Worker invariant: state was Ready when we popped. CAS
            // Ready → Running.
            assert!(
                self.state.enter_running(),
                "{}: slot was not Ready at run_cycle entry",
                self.label
            );

            let deadline = Instant::now() + budget.max_dur;
            let outcome = loop {
                if self.closed.load(Ordering::Acquire) && self.inbox_is_empty() {
                    break DrainOutcome::Closed;
                }
                let Some(env) = self.try_recv() else {
                    break DrainOutcome::Empty;
                };
                self.drain_one(env);
                let dispatched_this_cycle =
                    self.dispatched.load(Ordering::Acquire) % budget.max_mails.max(1);
                if dispatched_this_cycle == 0 || Instant::now() >= deadline {
                    // Mail count or wallclock budget hit. Yield.
                    break DrainOutcome::Yielded;
                }
                // Loop continues — drain next envelope.
            };

            match outcome {
                DrainOutcome::Empty => {
                    self.state.mark_idle();
                    if !self.inbox_is_empty() && self.state.try_self_requeue() {
                        CycleResult::Requeue
                    } else {
                        CycleResult::Idle
                    }
                }
                DrainOutcome::Yielded => {
                    self.state.mark_ready();
                    CycleResult::Requeue
                }
                DrainOutcome::Closed => {
                    self.state.mark_idle();
                    CycleResult::Closed
                }
            }
        }

        fn label(&self) -> &'static str {
            self.label
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Helper: drive one cycle on a single slot via the standard
    /// budget. Returns the cycle result.
    pub fn run_one_cycle(slot: &Arc<CounterSlot>) -> CycleResult {
        slot.run_cycle(BatchBudget::standard())
    }

    #[test]
    fn idle_to_ready_transition_signals_wake() {
        let state = SlotState::new();
        assert_eq!(state.current(), SlotStateLabel::Idle);
        assert!(state.try_wake(), "Idle → Ready CAS should win");
        assert_eq!(state.current(), SlotStateLabel::Ready);

        // Second wake attempt against Ready: no-op.
        assert!(
            !state.try_wake(),
            "second try_wake against Ready should fail"
        );
        assert_eq!(state.current(), SlotStateLabel::Ready);
    }

    #[test]
    fn enter_running_only_succeeds_from_ready() {
        let state = SlotState::new();
        assert!(!state.enter_running(), "cannot enter Running from Idle");
        state.try_wake();
        assert!(state.enter_running(), "Ready → Running should succeed");
        assert_eq!(state.current(), SlotStateLabel::Running);
        // Second enter_running against Running should fail (invariant
        // protection — only one worker drains a slot at a time).
        assert!(
            !state.enter_running(),
            "cannot re-enter Running from Running"
        );
    }

    #[test]
    fn seize_only_succeeds_from_idle() {
        // Demux-side `Idle → Running` (iamacoffeepot/aether#1135),
        // distinct from `enter_running`'s `Ready → Running`.
        let state = SlotState::new();
        assert_eq!(state.current(), SlotStateLabel::Idle);
        assert!(state.seize(), "Idle → Running seize should win");
        assert_eq!(state.current(), SlotStateLabel::Running);
        // A second seize against Running fails — only one runner.
        assert!(!state.seize(), "cannot seize a Running slot");
    }

    #[test]
    fn seize_fails_against_ready() {
        // A sender's `try_wake` already flipped the slot to `Ready`
        // (deposited mail, slot queued). The demuxer must lose the
        // seize and fall back to depositing — per-recipient FIFO is
        // then preserved by the inbox draining in send order.
        let state = SlotState::new();
        assert!(state.try_wake(), "Idle → Ready CAS should win");
        assert_eq!(state.current(), SlotStateLabel::Ready);
        assert!(!state.seize(), "cannot seize a Ready slot");
        assert_eq!(state.current(), SlotStateLabel::Ready);
    }

    #[test]
    fn drain_empty_returns_idle() {
        let slot = CounterSlot::new("empty");
        slot.push(1);
        slot.push(2);
        slot.state.try_wake();

        // Generous mail + wallclock budget so a single cycle drains both
        // envelopes regardless of CPU contention. This test validates the
        // state-machine invariant ("drain-to-empty leaves Idle"), not the
        // per-cycle budget — `drain_budget_yields_for_requeue` covers that.
        let budget = BatchBudget::custom(BATCH_MAX_MAILS, Duration::from_mins(1));
        let outcome = slot.run_cycle(budget);

        assert_eq!(outcome, CycleResult::Idle);
        assert_eq!(slot.dispatched(), 2);
        assert_eq!(slot.state.current(), SlotStateLabel::Idle);
    }

    #[test]
    fn drain_budget_yields_for_requeue() {
        let slot = CounterSlot::new("budget");
        for n in 0..(BATCH_MAX_MAILS + 50) {
            slot.push(n);
        }
        slot.state.try_wake();

        // Generous wallclock so only the count budget (`BATCH_MAX_MAILS`)
        // can trip — this test asserts the exact dispatched count after
        // the budget trips, so a 200μs wallclock (the `standard()`
        // default) racing the count under CI CPU contention would
        // dispatch some `N < BATCH_MAX_MAILS` and flake the assert
        // (iamacoffeepot/aether#869). Same workaround `drain_empty_returns_idle`
        // uses for the same reason.
        let budget = BatchBudget::custom(BATCH_MAX_MAILS, Duration::from_mins(1));
        let result = slot.run_cycle(budget);
        assert_eq!(result, CycleResult::Requeue);
        assert_eq!(slot.dispatched(), BATCH_MAX_MAILS);
        assert_eq!(slot.state.current(), SlotStateLabel::Ready);

        // Second cycle drains the rest. Same wallclock isolation — the
        // remaining 50 envelopes shouldn't trip the count budget but
        // could trip the 200μs wallclock under contention.
        let budget = BatchBudget::custom(BATCH_MAX_MAILS, Duration::from_mins(1));
        let result = slot.run_cycle(budget);
        assert_eq!(result, CycleResult::Idle);
        assert_eq!(slot.dispatched(), BATCH_MAX_MAILS + 50);
        assert_eq!(slot.state.current(), SlotStateLabel::Idle);
    }

    #[test]
    fn post_empty_recheck_requeues_on_concurrent_send() {
        // Simulate the race: drain to empty, then a "sender" pushes
        // before the recheck fires. The recheck should self-requeue.
        //
        // The load-bearing invariant is "no envelope is lost across
        // drain/push interleavings", not "one cycle suffices". The
        // loop covers both outcomes: a single-cycle drain (no budget
        // pressure) and a split-across-cycles drain (when
        // `BatchBudget::standard()`'s wallclock deadline trips between
        // envelopes under CPU contention — the cycle returns
        // `Requeue` after envelope 1, and a follow-up cycle drains
        // envelope 2). `Closed` would indicate a real bug.
        let slot = CounterSlot::new("recheck");
        slot.push(1);
        slot.state.try_wake();

        // Drain runs; we manually inject the race by pushing right
        // before the cycle returns. Since CounterSlot::run_cycle
        // already does the recheck inline, we simulate "push during
        // drain" by pushing just-in-time: push, then call run_cycle.
        slot.push(2);
        for _ in 0..8 {
            let result = run_one_cycle(&slot);
            match result {
                CycleResult::Idle | CycleResult::Requeue => {}
                CycleResult::Closed => {
                    panic!("unexpected Closed during recheck-requeue test")
                }
            }
            if slot.dispatched() == 2 {
                break;
            }
            // Ensure the slot is wakeable for the next cycle. After
            // an `Idle` result with envelopes still pending the state
            // is Idle; after `Requeue` it is already Ready. `try_wake`
            // is idempotent — no-op when already Ready (see
            // `wake_handle_pushes_to_ready_queue_once_per_idle`).
            slot.state.try_wake();
        }
        assert_eq!(
            slot.dispatched(),
            2,
            "both envelopes should drain across at most 8 cycles"
        );
    }

    #[test]
    fn closed_inbox_returns_closed() {
        let slot = CounterSlot::new("closed");
        slot.push(1);
        slot.state.try_wake();
        slot.close_inbox();

        // Generous wallclock so only inbox-closed-and-empty can decide
        // the cycle outcome — the standard 200μs wallclock can trip
        // between the two iterations of `CounterSlot`'s drain loop
        // (drain envelope 1 → try_recv sees closed && empty → Closed)
        // under CI CPU contention, flipping the result to `Requeue`
        // (iamacoffeepot/aether#896; sibling of iamacoffeepot/aether#869).
        let budget = BatchBudget::custom(BATCH_MAX_MAILS, Duration::from_mins(1));
        let result = slot.run_cycle(budget);
        // Closed + non-empty: drain remaining first, then return
        // Closed on next try_recv. CounterSlot's loop checks closed +
        // empty first, so the first iteration drains envelope 1, the
        // second iteration sees closed && empty → Closed.
        assert_eq!(result, CycleResult::Closed);
        assert_eq!(slot.dispatched(), 1);
    }

    #[test]
    fn wake_handle_pushes_to_ready_queue_once_per_idle() {
        // The test thread isn't a pool worker, so a wake spills straight
        // to the injector (the affinity own-deque path is skipped).
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let slot = CounterSlot::new("wake");
        let weak: Weak<dyn Drainable> = Arc::downgrade(&(slot.clone() as Arc<dyn Drainable>));
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()), 8);
        let wake = WakeHandle::new(slot.state.clone(), weak, sink);

        // First wake should spill one slot.
        assert!(wake.wake());
        // Second wake against Ready: no push.
        assert!(!wake.wake());
        // Exactly one entry in the injector (retry past transient steal
        // contention; Empty means the wake never spilled).
        let first = loop {
            match injector.steal() {
                Steal::Success(s) => break Some(s),
                Steal::Retry => {}
                Steal::Empty => break None,
            }
        };
        assert!(first.is_some(), "injector should hold the woken slot");
        assert!(
            !matches!(injector.steal(), Steal::Success(_)),
            "injector should not hold a duplicate"
        );
    }
}
