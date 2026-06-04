//! Spin-then-park worker coordinator — route-to-spinner dispatch
//! (iamacoffeepot/aether#1064).
//!
//! Replaces the worker's blocking `recv(ready_rx)` park with a
//! self-managed two-phase wait: a bounded **spin** phase (the worker
//! keeps scanning the ready queue, staying warm) followed by a
//! **park** phase (the worker blocks on its own thread token). The
//! point of taking ownership of the park is the producer-side gate:
//!
//! - With crossbeam's blocking `recv`, *every* `send` futex-wakes a
//!   blocked receiver (~4.3µs), regardless of whether another worker
//!   was already awake and able to pick the work up.
//! - Here, workers never block on the queue. A producer that just
//!   pushed work calls [`SpinPark::notify`], which **skips the unpark
//!   entirely when a worker is already spinning** — the spinner will
//!   scan the freshly-pushed slot with no futex involved. Only the
//!   genuine idle → first-event edge (no spinner available) pays a
//!   wakeup.
//!
//! This is the "route-to-spinner" lever. It differs from the
//! uncoordinated spin-before-park already tried and rejected
//! (iamacoffeepot/aether#1059): there the worker spun but the producer
//! still went through crossbeam's wake path, so the pool paid the spin
//! CPU *and* the wakeup. The win requires spin **with** the producer
//! consulting the spin count, not spin alone.
//!
//! ## Lost-wakeup safety
//!
//! The producer (`push; notify`) and a worker deciding to park
//! (`stop spinning; recheck; park`) form a classic `StoreLoad` race: if
//! the producer reads "a spinner exists" and skips the unpark, but
//! that spinner simultaneously stops spinning and parks without seeing
//! the work, the slot is stranded with every worker asleep.
//!
//! Two rules close it:
//!
//! 1. **Symmetric `SeqCst` fences.** The producer does
//!    `push → fence(SeqCst) → load(spinning)`; a parking worker does
//!    `store(spinning) → fence(SeqCst) → rescan`. The two fences are
//!    totally ordered, so at least one side observes the other's store
//!    (Dekker): either the producer sees the decremented count and
//!    unparks, or the worker's rescan sees the pushed work. The queue's
//!    own acquire/release ops compose with the fences, so a scan
//!    ordered after both fences sees a push ordered before both.
//! 2. **Register-before-decrement.** A worker about to park pushes its
//!    thread handle into the idle list *before* it decrements
//!    `spinning`. So at the instant the count reaches zero, the worker
//!    is already visible to a producer's `notify` pop — there is no
//!    window where the count is zero but the worker is unreachable.
//!
//! The producer-side `notify` fast path (a spinner exists) takes no
//! lock; only the slow path (no spinner → must unpark a specific parked
//! worker) touches the idle-list mutex. Parking is the path we optimise
//! *away*, so a lock there is fine.

use std::hint;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, Thread, ThreadId};
use std::time::{Duration, Instant};

/// Default spin-window length. The issue calls for "tens of µs": long
/// enough to absorb a fan-out spill or a back-to-back relay hop without
/// a futex wake, short enough that the idle tail between frames parks
/// quickly. Overridable per [`SpinPark::with_spin_window`] (the chassis
/// reads `AETHER_SPIN_WINDOW_USEC` so the latency sweep can retune
/// without a recompile).
pub const DEFAULT_SPIN_WINDOW_USEC: u64 = 50;

/// How often (in spin iterations) to re-read the wallclock. Reading
/// `Instant::now()` every iteration would dominate the loop; sampling
/// every `CLOCK_CHECK_STRIDE` iterations keeps the window bound tight
/// enough (the stride is a few hundred ns of polling) while leaving the
/// scan as the hot operation.
const CLOCK_CHECK_STRIDE: u32 = 64;

/// What [`SpinPark::acquire`] resolved to.
pub enum Acquired<T> {
    /// A slot was scanned out of the ready queue.
    Slot(T),
    /// Shutdown was signalled; the worker should exit its loop.
    Shutdown,
}

/// A parked worker, plus the cell a waker stamps with its `unpark` time so
/// the worker can fold the `notify → wake` latency on resume
/// (iamacoffeepot/aether#1182 Part 2 — dark handoff-cost refinement). The
/// stamp is nanos-since [`SpinPark::base`]; `0` means "no fresh stamp"
/// (spurious wake, or the worker self-served before parking), which folds
/// nothing. Diagnostic only — it rides alongside the `Thread` handle and
/// never gates a wakeup decision, so it cannot affect lost-wakeup safety.
#[derive(Debug)]
struct Parked {
    thread: Thread,
    stamp: Arc<AtomicU64>,
}

thread_local! {
    /// This worker thread's reusable `notify → wake` stamp cell
    /// (iamacoffeepot/aether#1182 Part 2). [`SpinPark::acquire`] clones the
    /// `Arc` into the idle list each park; the waker stamps it before
    /// `unpark`; the worker folds the latency on resume. Thread-local so a
    /// worker re-entering `acquire` pays an atomic refcount bump rather than
    /// a heap allocation. Only the owning worker ever clones it; a waker
    /// reaches it solely through the idle-list handoff.
    static WAKE_STAMP: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
}

/// Shared spin/park coordinator. One per [`crate::scheduler::Pool`],
/// held in an `Arc` and shared by every worker (the wait side) and by
/// every [`crate::scheduler::WakeHandle`] (the notify side).
#[derive(Debug)]
pub struct SpinPark {
    /// Workers currently in the spin phase (running, scanning the ready
    /// queue) — *not* parked, *not* processing. The route-to-spinner
    /// gate reads this: a producer that observes `spinning > 0` skips
    /// the unpark.
    spinning: AtomicUsize,
    /// Currently-parked workers, available to unpark. Only the notify slow
    /// path (no spinner) and the park path touch it — the fast path never
    /// locks. Each entry carries the worker's [`Parked::stamp`] so a waker
    /// can record its `unpark` time for the live handoff measurement.
    idle: Mutex<Vec<Parked>>,
    /// Set once at chassis teardown; observed by workers in both the
    /// spin loop and the park-commit recheck so they exit promptly.
    shutdown: AtomicBool,
    /// Bounded spin window — see [`DEFAULT_SPIN_WINDOW_USEC`].
    spin_window: Duration,
    /// Monotonic origin for the `notify → wake` stamps. Both the waker
    /// (`base.elapsed()` written into a [`Parked::stamp`] before `unpark`)
    /// and the woken worker (`base.elapsed()` on resume) measure against
    /// it, so their difference is the live handoff latency
    /// (iamacoffeepot/aether#1182 Part 2).
    base: Instant,
}

impl SpinPark {
    /// Construct with the default spin window.
    #[must_use]
    pub fn new() -> Self {
        Self::with_spin_window(Duration::from_micros(DEFAULT_SPIN_WINDOW_USEC))
    }

    /// Construct with an explicit spin window (chassis env override /
    /// tests).
    #[must_use]
    pub fn with_spin_window(spin_window: Duration) -> Self {
        Self {
            spinning: AtomicUsize::new(0),
            idle: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            spin_window,
            base: Instant::now(),
        }
    }

    /// Nanos since [`Self::base`], saturating into `u64` — the stamp unit
    /// shared by the waker (at `unpark`) and the woken worker (at resume).
    fn now_nanos(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn idle(&self) -> MutexGuard<'_, Vec<Parked>> {
        // A handler panic never unwinds while the idle lock is held
        // (the lock is only taken for the push/pop/remove book-keeping
        // here, never across a `run_cycle`), so poisoning shouldn't
        // happen — but recover rather than propagate so a poisoned lock
        // can't wedge the whole pool.
        self.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Producer side: call **after** pushing a slot onto the ready
    /// queue. Wakes a parked worker only if no worker is currently
    /// spinning (route-to-spinner). See the module docs for why the
    /// fence + register-before-decrement discipline makes this
    /// lost-wakeup-safe.
    pub fn notify(&self) {
        // StoreLoad barrier: the caller's queue push is sequenced before
        // this fence; the `spinning` load is sequenced after it. Paired
        // with the parking worker's fence (between its `spinning` store
        // and its rescan), this guarantees at least one side observes
        // the other.
        fence(Ordering::SeqCst);
        if self.spinning.load(Ordering::Relaxed) != 0 {
            // A spinner is scanning; it will pick the slot up with no
            // futex wake. This is the win.
            return;
        }
        // No spinner — wake one parked worker, if any. If none are
        // parked either, every worker is processing and will rescan
        // when it finishes its current cycle. Bind the pop result so the
        // idle-list guard is released before the `unpark` (and isn't a
        // live temporary across the `if let`).
        let parked = self.idle().pop();
        if let Some(p) = parked {
            // Stamp the worker's wake-latency cell just before the unpark
            // so it can fold the `notify → wake` cost on resume (Part 2).
            // Diagnostic only — ordered after the lost-wakeup discipline,
            // never part of it.
            p.stamp.store(self.now_nanos(), Ordering::Relaxed);
            p.thread.unpark();
        }
    }

    /// Producer side: call **after** pushing `n` slots, to wake up to `n`
    /// parked workers to drain them in parallel. Unlike [`Self::notify`],
    /// this does **not** route-to-spinner — a single spinner cannot drain
    /// `n` independent slots, so a batch producer must wake the parked
    /// siblings directly (iamacoffeepot/aether#1137 recruitment). Spinners
    /// still opportunistically scan the pushed slots, so the effective
    /// drainer count is the woken parked workers *plus* any spinners; over-
    /// waking is harmless (a worker that finds no work re-parks).
    ///
    /// Lost-wakeup-safe by the same discipline as [`Self::notify`]: the
    /// `SeqCst` fence orders the caller's pushes before the idle-list read,
    /// and the parking worker's register-before-decrement makes any worker
    /// that committed to parking visible to the pop. The unpark token is
    /// sticky, so a worker that re-enters its spin loop between our pop and
    /// its park still observes the work.
    pub fn wake_workers(&self, n: usize) {
        if n == 0 {
            return;
        }
        // StoreLoad barrier: pushes-before, idle-read-after (see `notify`).
        fence(Ordering::SeqCst);
        // Pop up to `n` parked handles under one lock, then unpark them all
        // after releasing the guard (no unpark while holding the lock).
        let mut woken: Vec<Parked> = Vec::new();
        {
            let mut idle = self.idle();
            for _ in 0..n {
                match idle.pop() {
                    Some(p) => woken.push(p),
                    None => break,
                }
            }
        }
        for p in woken {
            // Stamp each recruit's wake-latency cell before its unpark (see
            // `notify`); the woken worker folds the cost on resume (Part 2).
            p.stamp.store(self.now_nanos(), Ordering::Relaxed);
            p.thread.unpark();
        }
    }

    /// Worker side: wait for work. The caller has already tried its
    /// worker-local affinity cell and one non-blocking `scan`; this
    /// runs the spin-then-park loop until `scan` yields a slot or
    /// shutdown is signalled.
    ///
    /// `scan` is the non-blocking work probe (post-Phase-3a, a steal
    /// into the worker's own deque from the injector + siblings). It is
    /// called repeatedly — in the spin loop and once more in the
    /// park-commit recheck, which makes it the pre-park steal-rescan —
    /// so it must be cheap and must not block.
    pub fn acquire<T, F: Fn() -> Option<T>>(&self, scan: F) -> Acquired<T> {
        // This worker's reusable wake-latency cell (Part 2). Cloned from a
        // thread-local, so re-entering `acquire` is an atomic refcount bump,
        // not a heap allocation, on the idle path.
        let stamp = WAKE_STAMP.with(Arc::clone);
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return Acquired::Shutdown;
            }

            // Spin phase: stay warm, scanning the ready queue.
            self.spinning.fetch_add(1, Ordering::Relaxed);
            // Pair with the producer's notify fence: a producer that
            // pushed before our entry fence is visible to the scans
            // below.
            fence(Ordering::SeqCst);
            let found = self.spin(&scan);
            if let Some(slot) = found {
                self.spinning.fetch_sub(1, Ordering::Relaxed);
                return Acquired::Slot(slot);
            }

            // Park-commit: register in the idle list BEFORE decrementing
            // `spinning`, so the instant the count can read zero we are
            // already reachable by a producer's `notify` pop. Clear any
            // residue from a prior cycle's unconsumed stamp first, then
            // publish our handle + stamp cell together.
            let me = thread::current();
            stamp.store(0, Ordering::Relaxed);
            self.idle().push(Parked {
                thread: me.clone(),
                stamp: Arc::clone(&stamp),
            });
            self.spinning.fetch_sub(1, Ordering::Relaxed);
            // StoreLoad barrier mirroring the producer's: our `spinning`
            // store is before this fence, the recheck scan is after it.
            fence(Ordering::SeqCst);
            if let Some(slot) = scan() {
                self.remove_idle(me.id());
                return Acquired::Slot(slot);
            }
            if self.shutdown.load(Ordering::Acquire) {
                self.remove_idle(me.id());
                return Acquired::Shutdown;
            }

            // Genuinely idle: block. `thread::park` may return
            // spuriously and the unpark token is sticky (a notify that
            // raced ahead of this call returns immediately), so the
            // outer loop re-spins and rescans on every wake — no wakeup
            // is lost to a spurious return.
            thread::park();
            self.remove_idle(me.id());
            // Part 2 (dark): if a waker stamped us before its `unpark`,
            // fold the `notify → wake` latency into the live handoff
            // estimate. `swap(0)` consumes the stamp so a later spurious
            // wake on the sticky token folds nothing; a `0` (spurious wake,
            // or we were popped but self-served via the rescan above) is
            // skipped.
            let stamped = stamp.swap(0, Ordering::Relaxed);
            if stamped != 0 {
                let latency = self.now_nanos().saturating_sub(stamped);
                super::calibrate::fold_handoff_sample(latency);
            }
        }
    }

    /// The bounded spin: scan until a slot turns up, the window
    /// elapses, or shutdown. Returns the slot if found.
    fn spin<T, F: Fn() -> Option<T>>(&self, scan: &F) -> Option<T> {
        let deadline = Instant::now() + self.spin_window;
        let mut i: u32 = 0;
        loop {
            if let Some(slot) = scan() {
                return Some(slot);
            }
            i = i.wrapping_add(1);
            if i.is_multiple_of(CLOCK_CHECK_STRIDE)
                && (self.shutdown.load(Ordering::Relaxed) || Instant::now() >= deadline)
            {
                return None;
            }
            hint::spin_loop();
        }
    }

    fn remove_idle(&self, id: ThreadId) {
        let mut idle = self.idle();
        if let Some(pos) = idle.iter().position(|p| p.thread.id() == id) {
            idle.swap_remove(pos);
        }
    }

    /// Signal shutdown. Workers observe the flag in the spin loop and
    /// the park-commit recheck; any already blocked in `thread::park`
    /// are released by the caller unparking every worker thread
    /// (the [`crate::scheduler::Pool`] holds the join handles and
    /// unparks them on teardown).
    pub fn set_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

impl Default for SpinPark {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: a failed recv/join is the assertion"
)]
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
mod tests {
    use super::*;
    use crossbeam_channel::{Receiver, Sender, unbounded};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// A `Parked` entry for the current thread with a fresh (unstamped)
    /// wake cell — what `acquire`'s park-commit pushes, minus the stamp
    /// plumbing the unit tests don't exercise.
    fn parked_self() -> Parked {
        Parked {
            thread: thread::current(),
            stamp: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Poll `cond` up to `timeout`, sleeping between checks. Returns the
    /// final value of `cond`.
    fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        cond()
    }

    /// `notify` with a spinner present must not pop the idle list —
    /// the spinner is trusted to scan. With no spinner it pops and
    /// unparks a registered worker.
    #[test]
    fn notify_skips_unpark_when_spinning() {
        let coord = SpinPark::new();
        // Pretend a worker is spinning and another is parked.
        coord.spinning.fetch_add(1, Ordering::Relaxed);
        coord.idle().push(parked_self());

        coord.notify();
        // Spinner present → idle list untouched.
        assert_eq!(coord.idle().len(), 1, "notify must not pop while spinning");

        // Spinner leaves; now notify must route to the parked worker.
        coord.spinning.fetch_sub(1, Ordering::Relaxed);
        coord.notify();
        assert_eq!(coord.idle().len(), 0, "notify must pop when no spinner");
        // We unparked ourselves; consume the sticky token so it doesn't
        // leak into a later `park` in this test thread.
        thread::park_timeout(Duration::from_millis(0));
    }

    /// `wake_workers` must unpark up to `n` parked workers **even when a
    /// spinner is present** — unlike `notify`, it does not route-to-spinner
    /// (a lone spinner cannot drain `n` independent recruited clones). It
    /// also caps at the parked count rather than under/over-running.
    /// Regression guard for the recruit under-wake (iamacoffeepot/aether#1143).
    #[test]
    fn wake_workers_unparks_despite_spinner() {
        let coord = SpinPark::new();
        // A spinner is present (this would make `notify` skip the unpark),
        // and three workers are parked.
        coord.spinning.fetch_add(1, Ordering::Relaxed);
        for _ in 0..3 {
            coord.idle().push(parked_self());
        }

        coord.wake_workers(3);
        assert_eq!(
            coord.idle().len(),
            0,
            "wake_workers must unpark all n parked despite the spinner"
        );

        // n beyond the parked count is a no-op tail (no panic / underflow).
        coord.wake_workers(5);
        assert_eq!(coord.idle().len(), 0, "empty idle list stays empty");

        // Consume the sticky unpark token(s) delivered to this thread so
        // they don't leak into a later `park`.
        thread::park_timeout(Duration::from_millis(0));
    }

    /// Shutdown signalled while a worker is in `acquire` resolves to
    /// `Shutdown` rather than hanging.
    #[test]
    fn acquire_resolves_shutdown() {
        let coord = Arc::new(SpinPark::with_spin_window(Duration::from_micros(50)));
        let c2 = Arc::clone(&coord);
        let worker =
            thread::spawn(move || matches!(c2.acquire(|| None::<u32>), Acquired::Shutdown));
        // Let it cycle into the park, then signal + unpark.
        thread::sleep(Duration::from_millis(20));
        coord.set_shutdown();
        worker.thread().unpark();
        assert!(worker.join().unwrap(), "acquire should resolve Shutdown");
    }

    /// Contention/backoff-sensitive tests live in `mod heavy`: they exercise
    /// the spin-then-park backoff path, so they are serialized into the
    /// `serial-heavy` nextest group (`.config/nextest.toml`) to avoid
    /// oversubscribing cores against one another, and selected by
    /// `scripts/flake-soak.sh` for fresh-process soak repetition.
    mod heavy {
        use super::*;
        use crate::scheduler::calibrate;

        /// Part 2 (iamacoffeepot/aether#1182): a genuine parked-worker wake
        /// folds one live `notify → wake` sample into the handoff estimate. A
        /// worker parks (scan always empty), the main thread waits for it to
        /// register idle, then `notify`s — which stamps the worker and unparks
        /// it, so on resume it folds the measured latency. The folded-sample
        /// count must rise by at least one.
        #[test]
        fn parked_worker_folds_a_handoff_sample() {
            let before = calibrate::handoff_samples();
            let coord = Arc::new(SpinPark::with_spin_window(Duration::from_micros(20)));
            let c2 = Arc::clone(&coord);
            // The worker parks (scan never yields), folds on the notify wake,
            // re-spins, and exits on shutdown.
            let worker =
                thread::spawn(move || matches!(c2.acquire(|| None::<u32>), Acquired::Shutdown));

            // Wait until the worker has committed to the idle list — so the
            // `notify` below hits the park path (stamp + unpark), not the
            // route-to-spinner fast path (which folds nothing).
            assert!(
                wait_until(Duration::from_secs(2), || !coord.idle().is_empty()),
                "worker should register as parked",
            );
            coord.notify(); // stamp + unpark → the worker folds notify→wake
            // Give the woken worker time to fold before tearing down.
            assert!(
                wait_until(Duration::from_secs(2), || {
                    calibrate::handoff_samples() > before
                }),
                "a live parked-worker wake must fold a handoff sample",
            );

            coord.set_shutdown();
            worker.thread().unpark();
            assert!(worker.join().unwrap(), "acquire should resolve Shutdown");
        }

        /// Lost-wakeup stress: many producers push tokens onto a shared
        /// queue + `notify`; many workers `acquire`-drain. Every pushed
        /// token must be consumed exactly once — a stranded token (work in
        /// the queue, all workers parked, no pending unpark) hangs the
        /// drain and trips the timeout. This is the highest-risk surface.
        #[test]
        fn lost_wakeup_stress() {
            const WORKERS: usize = 6;
            const PRODUCERS: usize = 4;
            const PER_PRODUCER: u64 = 5_000;

            let coord = Arc::new(SpinPark::with_spin_window(Duration::from_micros(30)));
            let (tx, rx): (Sender<u64>, Receiver<u64>) = unbounded();
            let consumed = Arc::new(AtomicU64::new(0));
            let total = PRODUCERS as u64 * PER_PRODUCER;

            let workers: Vec<_> = (0..WORKERS)
                .map(|_| {
                    let coord = Arc::clone(&coord);
                    let rx = rx.clone();
                    let consumed = Arc::clone(&consumed);
                    thread::spawn(move || {
                        loop {
                            match coord.acquire(|| rx.try_recv().ok()) {
                                Acquired::Slot(_) => {
                                    consumed.fetch_add(1, Ordering::Relaxed);
                                }
                                Acquired::Shutdown => return,
                            }
                        }
                    })
                })
                .collect();

            let producers: Vec<_> = (0..PRODUCERS)
                .map(|p| {
                    let coord = Arc::clone(&coord);
                    let tx = tx.clone();
                    thread::spawn(move || {
                        for n in 0..PER_PRODUCER {
                            tx.send(p as u64 * PER_PRODUCER + n).unwrap();
                            coord.notify();
                        }
                    })
                })
                .collect();
            for p in producers {
                p.join().unwrap();
            }

            // Wait for full drain — a lost wakeup strands the tail and this
            // times out.
            let start = Instant::now();
            while consumed.load(Ordering::Relaxed) < total {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "drain stalled at {}/{} — likely a lost wakeup",
                    consumed.load(Ordering::Relaxed),
                    total,
                );
                thread::sleep(Duration::from_millis(5));
            }

            coord.set_shutdown();
            for w in &workers {
                w.thread().unpark();
            }
            for w in workers {
                w.join().unwrap();
            }
            assert_eq!(consumed.load(Ordering::Relaxed), total);
        }
    }
}
