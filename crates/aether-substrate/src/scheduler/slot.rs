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
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

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
    /// Wallclock deadline. The slot stops draining once `Instant::now()`
    /// is past this point (checked between envelopes).
    pub deadline: Instant,
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
            deadline: Instant::now() + max_duration,
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
/// PR B exercises it via the in-module test fixture
/// [`tests::CounterSlot`].
///
/// Implementors own:
/// - The per-slot [`SlotState`] (so this trait can poll it).
/// - The actor's inbox (so [`run_cycle`](Self::run_cycle) can drain).
/// - Whatever the actor's handler invocation needs (a `Box<A>` plus
///   the per-envelope wrapping that [`crate::actor::native::dispatch::dispatch_loop_run`]
///   does — `local::with_stamped`, `log_install::with_actor_dispatch`,
///   etc).
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
    /// [`crate::actor::native::spawn::Spawner::shutdown_instanced`]
    /// uses this to settle on each spawned slot via `recv_timeout`
    /// instead of polling [`Self::is_closed`] in a 2 ms loop, which
    /// flaked under nextest contention.
    ///
    /// Default no-op: mock fixtures don't have a real close cycle so
    /// they never need to signal. Idempotent — the slot only fires the
    /// first installed sender once; a re-install after close is a no-op.
    fn set_close_done_tx(&self, _tx: crossbeam_channel::Sender<()>) {}

    /// Upcast helper for downcasting in tests. Production code doesn't
    /// reach for this.
    fn as_any(&self) -> &dyn Any;
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
    ready_tx: Sender<Arc<dyn Drainable>>,
}

impl WakeHandle {
    /// Construct a wake handle. The chassis registry calls this when
    /// it wires a new dispatcher slot.
    pub fn new(
        state: Arc<SlotState>,
        slot: Weak<dyn Drainable>,
        ready_tx: Sender<Arc<dyn Drainable>>,
    ) -> Self {
        Self {
            state,
            slot,
            ready_tx,
        }
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
        // Ready queue closed = pool is shutting down; treat as a no-op
        // rather than panicking.
        let _ = self.ready_tx.send(slot);
        true
    }

    /// Borrow the slot state. Tests reach for this; production code
    /// goes through `wake`.
    #[must_use]
    pub fn state(&self) -> &Arc<SlotState> {
        &self.state
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::any::Any;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use std::time::Duration;

    use crossbeam_channel::unbounded;

    /// Test fixture: a slot with a `Vec<u32>` inbox and a counter
    /// incremented per dispatch. Exercises the [`Drainable`] surface
    /// without dragging in the real chassis machinery.
    pub struct CounterSlot {
        pub state: Arc<SlotState>,
        pub inbox: Mutex<std::collections::VecDeque<u32>>,
        pub closed: AtomicBool,
        pub dispatched: AtomicU32,
        /// If `Some(n)`, the n-th dispatch (1-indexed) panics. Used by
        /// the panic-isolation test in the `pool` module.
        pub panic_at: Option<u32>,
        /// Per-envelope work duration. Used by the time-budget test.
        pub work_per_env: Duration,
        pub label: &'static str,
    }

    impl CounterSlot {
        pub fn new(label: &'static str) -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(SlotState::new()),
                inbox: Mutex::new(std::collections::VecDeque::new()),
                closed: AtomicBool::new(false),
                dispatched: AtomicU32::new(0),
                panic_at: None,
                work_per_env: Duration::ZERO,
                label,
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
            assert!(
                Some(n) != self.panic_at,
                "CounterSlot panic at envelope #{n} (test-induced)"
            );
            std::hint::black_box(env);
            if !self.work_per_env.is_zero() {
                std::thread::sleep(self.work_per_env);
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
                if dispatched_this_cycle == 0 || Instant::now() >= budget.deadline {
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
    fn drain_empty_returns_idle() {
        let slot = CounterSlot::new("empty");
        slot.push(1);
        slot.push(2);
        slot.state.try_wake();

        // Generous mail + wallclock budget so a single cycle drains both
        // envelopes regardless of CPU contention. This test validates the
        // state-machine invariant ("drain-to-empty leaves Idle"), not the
        // per-cycle budget — `drain_budget_yields_for_requeue` covers that.
        let budget = BatchBudget::custom(BATCH_MAX_MAILS, Duration::from_secs(60));
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
        let budget = BatchBudget::custom(BATCH_MAX_MAILS, Duration::from_secs(60));
        let result = slot.run_cycle(budget);
        assert_eq!(result, CycleResult::Requeue);
        assert_eq!(slot.dispatched(), BATCH_MAX_MAILS);
        assert_eq!(slot.state.current(), SlotStateLabel::Ready);

        // Second cycle drains the rest. Same wallclock isolation — the
        // remaining 50 envelopes shouldn't trip the count budget but
        // could trip the 200μs wallclock under contention.
        let budget = BatchBudget::custom(BATCH_MAX_MAILS, Duration::from_secs(60));
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

        let result = run_one_cycle(&slot);
        // Closed + non-empty: drain remaining first, then return
        // Closed on next try_recv. CounterSlot's loop checks closed +
        // empty first, so the first iteration drains envelope 1, the
        // second iteration sees closed && empty → Closed.
        assert_eq!(result, CycleResult::Closed);
        assert_eq!(slot.dispatched(), 1);
    }

    #[test]
    fn wake_handle_pushes_to_ready_queue_once_per_idle() {
        let (ready_tx, ready_rx) = unbounded::<Arc<dyn Drainable>>();
        let slot = CounterSlot::new("wake");
        let weak: Weak<dyn Drainable> = Arc::downgrade(&(slot.clone() as Arc<dyn Drainable>));
        let wake = WakeHandle::new(slot.state.clone(), weak, ready_tx);

        // First wake should push.
        assert!(wake.wake());
        // Second wake against Ready: no push.
        assert!(!wake.wake());
        // Drain the queue: exactly one entry.
        let _drained = ready_rx
            .recv_timeout(Duration::from_millis(50))
            .expect("queue should hold the woken slot");
        assert!(
            ready_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "queue should not hold a duplicate"
        );
    }
}
