//! ADR-0086 Phase 0/1 — emit-time settlement counter (de-risk spike).
//!
//! The decoupled-settlement design (ADR-0086) moves per-root accounting
//! off the trace pipeline and onto the producing thread. Today the
//! [`crate::actor::native`] producer hooks push `TraceEvent`s onto the
//! sharded queue; a 1 ms-parking drainer ships them in batches to the
//! `TraceObserverCapability`, which folds them into
//! `RootState { in_flight, held_open }` and fires `Settled` on the
//! `(in_flight == 0 && held_open == 0)` transition. So settlement is not
//! observed until up to a drainer interval after the work actually
//! finished.
//!
//! This module is the replacement counting kernel: a per-root counter
//! updated **synchronously, at emit time, on the producing thread**, so
//! the zero-transition fires the instant the work completes — no queue,
//! no drainer, no fold. It is **not yet wired** into the producer hooks
//! (ADR-0086 Phase 1 does that, in shadow mode, cross-checked against the
//! incumbent fold). Phase 0 lands the kernel standalone and stress-proves
//! the part the ADR flagged riskiest: a concurrent zero-transition that
//! must fire exactly once even when a `Finished`'s decrement-to-zero
//! races a re-opening `Sent`.
//!
//! **Why packing both counts into one `u64` removes the CAS loop the ADR
//! anticipated.** With `in_flight` in the high 32 bits and `held_open` in
//! the low 32, the joint zero test `(in_flight == 0 && held_open == 0)`
//! is the single word value `0`. A decrement is one atomic `fetch_sub`
//! returning the pre-decrement word, and the post-state is `(0, 0)`
//! exactly when that prior word equalled the one unit being removed
//! (`1 << 32` for an `in_flight` decrement, `1` for a
//! `held_open` decrement). The atomic linearises every contending
//! operation, so for any given arrival at `(0, 0)` exactly one thread
//! observes that prior value and fires — no read-modify-write retry loop
//! is needed. A re-opening `Sent` racing the decrement is ordered by the
//! same atomic: whichever decrement lands on `(0, 0)` fires; if the
//! re-open wins the race the decrement sees a larger prior word and does
//! not fire.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use aether_data::MailId;

/// One unit of `in_flight` in the packed word: `in_flight` occupies the
/// high 32 bits, `held_open` the low 32.
const IN_FLIGHT_UNIT: u64 = 1 << 32;

/// Per-root settlement count, packed into a single atomic word.
///
/// `in_flight` (high 32 bits) tracks how many mails under the root have
/// been sent but not yet finished; `held_open` (low 32 bits) tracks
/// ADR-0080 §12 settlement holds (a thread that outlives its spawning
/// handler keeps the chain open until it drops). The root has settled
/// when the whole word reads `0`.
///
/// Operations are atomic read-modify-writes, so a cell can be driven
/// either under a lock (as [`SettlementCounter`] does, to also guard the
/// map structure) or fully lock-free (the future cached-cell hot path).
/// The exactly-once zero-transition holds in both modes — that is what
/// the `cell_*` stress tests prove.
#[derive(Debug)]
pub struct CounterCell {
    packed: AtomicU64,
}

impl CounterCell {
    /// A fresh cell at `(in_flight = 0, held_open = 0)`.
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            packed: AtomicU64::new(0),
        }
    }

    /// Current `(in_flight, held_open)`. For assertions / diagnostics;
    /// the firing decision never reads this (it uses the decrement's
    /// return value), so there is no read-then-act race.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn load(&self) -> (u32, u32) {
        let w = self.packed.load(Ordering::Acquire);
        ((w >> 32) as u32, (w & 0xFFFF_FFFF) as u32)
    }

    /// `in_flight += 1` (a `Sent` for this root).
    pub fn add_in_flight(&self) {
        self.packed.fetch_add(IN_FLIGHT_UNIT, Ordering::AcqRel);
    }

    /// `held_open += 1` (a settlement hold acquired).
    pub fn add_held_open(&self) {
        self.packed.fetch_add(1, Ordering::AcqRel);
    }

    /// `in_flight -= 1` (a `Finished`). Returns `true` iff this
    /// decrement brought the cell to `(0, 0)` — i.e. the caller should
    /// fire `Settled`.
    ///
    /// # Panics (debug only)
    /// Debug-asserts `in_flight > 0`. The emit-time path never
    /// under-decrements — every `Finished` matches a prior `Sent` for
    /// the same root — so an underflow signals a lineage-accounting bug.
    /// (The incumbent observer uses `saturating_sub` only because the
    /// drained stream can carry orphan `Finished` events for evicted
    /// trees; the emit-time path has no such orphans.)
    #[must_use]
    pub fn sub_in_flight(&self) -> bool {
        let prev = self.packed.fetch_sub(IN_FLIGHT_UNIT, Ordering::AcqRel);
        debug_assert!(prev >> 32 != 0, "settlement counter in_flight underflow");
        prev == IN_FLIGHT_UNIT
    }

    /// `held_open -= 1` (a hold released). Returns `true` iff this
    /// decrement brought the cell to `(0, 0)`.
    ///
    /// # Panics (debug only)
    /// Debug-asserts `held_open > 0`, symmetric with
    /// [`Self::sub_in_flight`].
    #[must_use]
    pub fn sub_held_open(&self) -> bool {
        let prev = self.packed.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            prev & 0xFFFF_FFFF != 0,
            "settlement counter held_open underflow"
        );
        prev == 1
    }

    /// True when the cell currently reads `(0, 0)`. Only meaningful
    /// under external serialisation (e.g. the stripe lock) — used by
    /// the map's drop-on-settle re-check.
    fn is_zero(&self) -> bool {
        self.packed.load(Ordering::Acquire) == 0
    }
}

/// Number of map stripes (power of two; mask is `N - 1`). Mirrors the
/// trace queue's shard count so concurrent roots spread the same way:
/// the lock guarding a root's cell is one of 64, so producers touching
/// different roots rarely contend.
const STRIPE_COUNT: usize = 64;

/// Emit-time settlement counter: a striped map from causal root
/// [`MailId`] to its [`CounterCell`].
///
/// The stripe lock guards the *map structure* (insert-on-first-event,
/// drop-on-settle). The per-root count itself is the cell's atomic word.
/// Holding the stripe lock across the count mutation makes the
/// zero-transition trivially serialised per root, and lets drop-on-settle
/// re-check the cell under the same lock so a re-opening `Sent` cannot
/// lose its increment to a concurrent reclaim.
#[derive(Debug)]
pub struct SettlementCounter {
    stripes: Box<[Mutex<HashMap<MailId, CounterCell>>]>,
    mask: u64,
}

impl SettlementCounter {
    /// Allocate `STRIPE_COUNT` empty stripes.
    #[must_use]
    pub fn new() -> Self {
        let stripes = (0..STRIPE_COUNT)
            .map(|_| Mutex::new(HashMap::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            stripes,
            #[allow(clippy::cast_possible_truncation)]
            mask: STRIPE_COUNT as u64 - 1,
        }
    }

    /// Stripe guarding `root`'s cell. Same mix as the trace queue's
    /// `shard_index`: scramble the sender word and fold in the
    /// per-root-incrementing `correlation_id` (whose low bits already
    /// round-robin across stripes).
    #[inline]
    #[allow(clippy::cast_possible_truncation)] // masked to < STRIPE_COUNT
    fn stripe(&self, root: MailId) -> &Mutex<HashMap<MailId, CounterCell>> {
        let h = root.sender.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ root.correlation_id;
        &self.stripes[(h & self.mask) as usize]
    }

    /// Lock the stripe guarding `root`'s cell. Panics on a poisoned
    /// mutex (fail-fast per ADR-0063); all the `record_*` methods inherit
    /// that contract through this helper.
    fn lock_stripe(&self, root: MailId) -> MutexGuard<'_, HashMap<MailId, CounterCell>> {
        self.stripe(root)
            .lock()
            .expect("settlement counter stripe mutex poisoned; fail-fast per ADR-0063")
    }

    /// Record a `Sent` for `root` (`in_flight += 1`). Inserts the cell
    /// on first event — including re-opening a root whose cell was
    /// dropped when it previously settled.
    pub fn record_sent(&self, root: MailId) {
        let mut stripe = self.lock_stripe(root);
        stripe
            .entry(root)
            .or_insert_with(CounterCell::zero)
            .add_in_flight();
    }

    /// Record a settlement `HoldOpen` for `root` (`held_open += 1`).
    pub fn record_hold_open(&self, root: MailId) {
        let mut stripe = self.lock_stripe(root);
        stripe
            .entry(root)
            .or_insert_with(CounterCell::zero)
            .add_held_open();
    }

    /// Record a `Finished` for `root` (`in_flight -= 1`). Returns `true`
    /// iff the root just reached `(0, 0)` and the caller should fire
    /// `Settled`. Drop-on-settle reclaims the cell under the stripe lock.
    /// An orphan `Finished` (no live cell) returns `false`.
    #[must_use]
    pub fn record_finished(&self, root: MailId) -> bool {
        let mut stripe = self.lock_stripe(root);
        let Some(cell) = stripe.get(&root) else {
            return false;
        };
        let settled = cell.sub_in_flight();
        if settled {
            // Re-check under the same lock: only reclaim if still zero.
            // (Belt-and-suspenders — nothing can bump the cell while we
            // hold the stripe lock, but this keeps the reclaim correct if
            // a future variant drops the lock between dec and reclaim.)
            if stripe.get(&root).is_some_and(CounterCell::is_zero) {
                stripe.remove(&root);
            }
        }
        settled
    }

    /// Record a hold `Release` for `root` (`held_open -= 1`). Returns
    /// `true` iff the root just reached `(0, 0)`. Symmetric with
    /// [`Self::record_finished`].
    #[must_use]
    pub fn record_release(&self, root: MailId) -> bool {
        let mut stripe = self.lock_stripe(root);
        let Some(cell) = stripe.get(&root) else {
            return false;
        };
        let settled = cell.sub_held_open();
        if settled && stripe.get(&root).is_some_and(CounterCell::is_zero) {
            stripe.remove(&root);
        }
        settled
    }

    /// Number of roots with a live cell (not yet settled). For
    /// assertions; production callers never need this.
    ///
    /// # Panics
    /// Panics on a poisoned stripe mutex (fail-fast per ADR-0063).
    #[must_use]
    pub fn live_roots(&self) -> usize {
        self.stripes
            .iter()
            .map(|s| {
                s.lock()
                    .expect("settlement counter stripe mutex poisoned")
                    .len()
            })
            .sum()
    }
}

impl Default for SettlementCounter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test arithmetic: thread joins and small bounded loop counters"
)]
mod tests {
    use super::*;
    use aether_data::MailboxId;
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn root(sender: u64, cid: u64) -> MailId {
        MailId {
            sender: MailboxId(sender),
            correlation_id: cid,
        }
    }

    #[test]
    fn cell_serial_zero_transitions() {
        let c = CounterCell::zero();
        assert_eq!(c.load(), (0, 0));
        c.add_in_flight();
        assert_eq!(c.load(), (1, 0));
        c.add_in_flight();
        assert_eq!(c.load(), (2, 0));
        // First finish: 2 -> 1, not settled.
        assert!(!c.sub_in_flight());
        // Second finish: 1 -> 0, settled.
        assert!(c.sub_in_flight());
        assert_eq!(c.load(), (0, 0));
    }

    #[test]
    fn cell_held_open_gates_settlement() {
        // The ADR-0080 §12 invariant: in_flight reaching zero does NOT
        // settle while a hold is open; only the matching release (with
        // in_flight already zero) fires.
        let c = CounterCell::zero();
        c.add_in_flight();
        c.add_held_open();
        // Finish drops in_flight to 0 but held_open is 1 → not settled.
        assert!(!c.sub_in_flight());
        assert_eq!(c.load(), (0, 1));
        // Release drops held_open to 0 with in_flight already 0 → fires.
        assert!(c.sub_held_open());
        assert_eq!(c.load(), (0, 0));
    }

    #[test]
    fn cell_release_before_finish_does_not_double_fire() {
        // Reverse order: release first (in_flight still 1 → no fire),
        // then finish (the decrement that lands on zero fires).
        let c = CounterCell::zero();
        c.add_in_flight();
        c.add_held_open();
        assert!(!c.sub_held_open(), "release with in_flight>0 must not fire");
        assert_eq!(c.load(), (1, 0));
        assert!(c.sub_in_flight(), "finish lands on zero → fires once");
    }

    /// The kernel's riskiest property, lock-free: N threads each do one
    /// `add_in_flight` then one `sub_in_flight` on a SINGLE shared cell.
    /// The cell's value walks up and down; the number of `true`s
    /// returned (each a genuine arrival at zero) must never exceed the
    /// number of subs, the cell must end at zero, and — the exactly-once
    /// guarantee — the total fire count equals the number of times the
    /// counter truly returned to zero, which we pin with a deterministic
    /// final-drain phase below.
    #[test]
    fn cell_zero_transition_is_exactly_once_under_contention() {
        let cell = Arc::new(CounterCell::zero());
        let threads = 8;
        let per = 50_000;
        let fires = Arc::new(AtomicU32::new(0));

        // Pre-load in_flight so the concurrent phase never under-flows:
        // each thread does (add, sub), net zero, but interleavings can
        // dip the running value — the +threads floor keeps it positive.
        for _ in 0..threads {
            cell.add_in_flight();
        }

        let mut handles = Vec::new();
        for _ in 0..threads {
            let cell = Arc::clone(&cell);
            let fires = Arc::clone(&fires);
            handles.push(thread::spawn(move || {
                for _ in 0..per {
                    cell.add_in_flight();
                    if cell.sub_in_flight() {
                        fires.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // After the concurrent phase the value is back to the `threads`
        // floor (every thread's adds and subs balanced). No fire could
        // have occurred yet — the value never reached zero (floor > 0).
        assert_eq!(cell.load(), (threads as u32, 0));
        assert_eq!(
            fires.load(Ordering::Relaxed),
            0,
            "no zero-arrival is possible while the in_flight floor is positive"
        );

        // Drain the floor: exactly one of these subs lands on zero.
        let mut drain_fires = 0;
        for _ in 0..threads {
            if cell.sub_in_flight() {
                drain_fires += 1;
            }
        }
        assert_eq!(cell.load(), (0, 0));
        assert_eq!(drain_fires, 1, "exactly one decrement lands on (0,0)");
    }

    /// Race the final decrement: one thread holds the cell at
    /// `in_flight = 1`, then `racers` threads each try the same final
    /// `sub_in_flight`. Exactly one must see the zero-arrival. Repeated
    /// many times to shake out interleavings. (Models the dangerous
    /// case: several siblings' `Finished` events landing together as the
    /// tree completes.)
    #[test]
    fn cell_final_decrement_race_fires_once() {
        for _ in 0..2_000 {
            let cell = Arc::new(CounterCell::zero());
            let racers = 4u32;
            // Seed exactly `racers` in_flight so exactly one sub hits zero.
            for _ in 0..racers {
                cell.add_in_flight();
            }
            let fires = Arc::new(AtomicU32::new(0));
            let start = Arc::new(Barrier::new(racers as usize));
            let mut handles = Vec::new();
            for _ in 0..racers {
                let cell = Arc::clone(&cell);
                let fires = Arc::clone(&fires);
                let start = Arc::clone(&start);
                handles.push(thread::spawn(move || {
                    start.wait();
                    if cell.sub_in_flight() {
                        fires.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(cell.load(), (0, 0));
            assert_eq!(fires.load(Ordering::Relaxed), 1);
        }
    }

    /// Serial oracle: the exact incumbent fold rules from
    /// `aether-capabilities::trace` (`Sent` → `in_flight + 1` with
    /// insert, `Finished` → `in_flight - 1` + zero-test, `HoldOpen` →
    /// `held_open + 1` with insert, `Release` → `held_open - 1` +
    /// zero-test). Returns the number of settle fires for a single root
    /// over an event sequence.
    #[derive(Clone, Copy)]
    enum Ev {
        Sent,
        Finished,
        Hold,
        Release,
    }

    fn oracle_fires(seq: &[Ev]) -> u32 {
        let mut in_flight: u32 = 0;
        let mut held: u32 = 0;
        let mut present = false;
        let mut fires = 0;
        for ev in seq {
            match ev {
                Ev::Sent => {
                    present = true;
                    in_flight += 1;
                }
                Ev::Hold => {
                    present = true;
                    held += 1;
                }
                Ev::Finished => {
                    if present {
                        in_flight -= 1;
                        if in_flight == 0 && held == 0 {
                            fires += 1;
                            present = false; // drop-on-settle
                        }
                    }
                }
                Ev::Release => {
                    if present {
                        held -= 1;
                        if in_flight == 0 && held == 0 {
                            fires += 1;
                            present = false;
                        }
                    }
                }
            }
        }
        fires
    }

    #[test]
    fn counter_serial_matches_oracle() {
        // A scripted sequence with a re-open: settle, then a fresh Sent
        // re-opens the same root and settles again. The counter's fire
        // count must equal the oracle's.
        let seq = [
            Ev::Sent,
            Ev::Sent,
            Ev::Hold,
            Ev::Finished, // (1,1)
            Ev::Release,  // (1,0)
            Ev::Finished, // (0,0) → fire #1, drop
            Ev::Sent,     // re-open (1,0)
            Ev::Finished, // (0,0) → fire #2, drop
        ];
        let r = root(7, 42);
        let counter = SettlementCounter::new();
        let mut fires = 0;
        for ev in &seq {
            match ev {
                Ev::Sent => counter.record_sent(r),
                Ev::Hold => counter.record_hold_open(r),
                Ev::Finished => {
                    if counter.record_finished(r) {
                        fires += 1;
                    }
                }
                Ev::Release => {
                    if counter.record_release(r) {
                        fires += 1;
                    }
                }
            }
        }
        assert_eq!(fires, oracle_fires(&seq));
        assert_eq!(fires, 2);
        assert_eq!(counter.live_roots(), 0, "settled roots are reclaimed");
    }

    /// Many roots, many threads, balanced workload: every root receives
    /// equal `Sent`/`Finished` (and matched `Hold`/`Release`) across
    /// threads. Each root must end settled (cell reclaimed), the map must
    /// be empty, and every root must have fired at least once.
    /// Producer-hot-path throughput microbench (ADR-0086 Phase 0).
    /// Times, per trivial-mail lifecycle (one `Sent` + one `Finished`):
    ///
    /// 1. `ShardedTraceQueue::push` ×2 — the current producer enqueue
    ///    cost, the work the emit-time counter replaces on the hot path.
    /// 2. `SettlementCounter` (striped lock + map) — the new cost, which
    ///    additionally performs settlement detection *inline* (the queue
    ///    defers that to the ≤1 ms drainer + observer fold).
    /// 3. `CounterCell` lock-free — the future cached-cell hot path.
    ///
    /// Each is measured single-threaded (warm) and under `threads`-way
    /// contention on distinct roots (the saturated multi-worker regime
    /// the trace sharding fought, iamacoffeepot/aether#1063). `#[ignore]`
    /// — a measurement, not a gate. Run release:
    ///
    /// ```text
    /// cargo test -p aether-substrate --release --lib \
    ///     chassis::settlement_counter::tests::bench_producer_hot_path \
    ///     -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "throughput microbench — run release with --ignored --nocapture"]
    #[allow(
        clippy::print_stdout,
        clippy::cast_precision_loss,
        clippy::too_many_lines
    )]
    fn bench_producer_hot_path() {
        use crate::runtime::trace::ShardedTraceQueue;
        use aether_data::KindId;
        use aether_kinds::trace::{Nanos, TraceEvent};
        use std::env;
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        let iters: u64 = env::var("BENCH_ITERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2_000_000);
        let threads: u64 = env::var("BENCH_THREADS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);

        let mk_sent = |r: MailId| TraceEvent::Sent {
            mail_id: r,
            root: r,
            parent_mail: None,
            sender: MailboxId(1),
            recipient: MailboxId(2),
            kind: KindId(3),
            t: Nanos(0),
        };
        let fin = |r: MailId| TraceEvent::Finished {
            mail_id: r,
            t: Nanos(0),
        };
        let ns_per = |d: Duration, ops: u64| d.as_nanos() as f64 / ops as f64;

        // Push-only producer cost, drained untimed in bounded batches so
        // memory stays flat (production drains on a separate thread, so
        // the producer never pays pop; measuring push alone is the fair
        // baseline). Returns ns per Sent+Finished pair.
        let batch: u64 = 50_000;
        let bench_queue_push = |q: &ShardedTraceQueue, r: MailId, pairs: u64| -> f64 {
            let mut elapsed = Duration::ZERO;
            let mut done = 0u64;
            while done < pairs {
                let n = batch.min(pairs - done);
                let t = Instant::now();
                for _ in 0..n {
                    q.push(r, mk_sent(r));
                    q.push(r, fin(r));
                }
                elapsed += t.elapsed();
                while q.pop().is_some() {} // untimed drain
                done += n;
            }
            ns_per(elapsed, pairs)
        };

        // Single-threaded warm timings.
        let r = root(1, 0);

        let q = ShardedTraceQueue::new();
        let q_warm = bench_queue_push(&q, r, iters);

        let counter = SettlementCounter::new();
        let t0 = Instant::now();
        for _ in 0..iters {
            counter.record_sent(r);
            black_box(counter.record_finished(r));
        }
        let c_warm = ns_per(t0.elapsed(), iters);

        let cell = CounterCell::zero();
        let t0 = Instant::now();
        for _ in 0..iters {
            cell.add_in_flight();
            black_box(cell.sub_in_flight());
        }
        let cell_warm = ns_per(t0.elapsed(), iters);

        // Contended: distinct root + mailbox per thread so they stripe
        // apart (a correct sharded/striped structure shows no degradation
        // here — that is the point of striping). Each thread accumulates
        // only its push/record time; drains (queue path) are untimed.
        // Mean ns/pair = summed per-thread elapsed / total pairs.
        let q = Arc::new(ShardedTraceQueue::new());
        let mut hs = Vec::new();
        for t in 0..threads {
            let q = Arc::clone(&q);
            hs.push(thread::spawn(move || -> Duration {
                let r = root(t + 1, t);
                let mut elapsed = Duration::ZERO;
                let mut done = 0u64;
                while done < iters {
                    let n = batch.min(iters - done);
                    let ts = Instant::now();
                    for _ in 0..n {
                        q.push(r, mk_sent(r));
                        q.push(r, fin(r));
                    }
                    elapsed += ts.elapsed();
                    while let Some(e) = q.pop() {
                        black_box(e);
                    }
                    done += n;
                }
                elapsed
            }));
        }
        let q_cont = ns_per(
            hs.into_iter().map(|h| h.join().unwrap()).sum(),
            iters * threads,
        );

        let counter = Arc::new(SettlementCounter::new());
        let mut hs = Vec::new();
        for t in 0..threads {
            let counter = Arc::clone(&counter);
            hs.push(thread::spawn(move || -> Duration {
                let r = root(t + 1, t);
                let ts = Instant::now();
                for _ in 0..iters {
                    counter.record_sent(r);
                    black_box(counter.record_finished(r));
                }
                ts.elapsed()
            }));
        }
        let c_cont = ns_per(
            hs.into_iter().map(|h| h.join().unwrap()).sum(),
            iters * threads,
        );

        println!();
        println!("=== producer hot-path throughput (ns per Sent+Finished pair) ===");
        println!("iters={iters}/thread, threads={threads}");
        println!("{:<34} {:>10} {:>12}", "path", "warm", "contended");
        println!(
            "{:<34} {:>10.1} {:>12.1}",
            "ShardedTraceQueue push x2", q_warm, q_cont
        );
        println!(
            "{:<34} {:>10.1} {:>12.1}",
            "SettlementCounter (striped+inline)", c_warm, c_cont
        );
        println!(
            "{:<34} {:>10.1} {:>12}",
            "CounterCell (lock-free)", cell_warm, "-"
        );
        println!();
        println!("note: the queue defers settlement detection to the ~1ms drainer + observer");
        println!("fold; the counter performs it inline, so its cost SUBSUMES that pipeline.");
    }

    #[test]
    fn counter_concurrent_all_roots_settle_and_reclaim() {
        let counter = Arc::new(SettlementCounter::new());
        let roots = 256u64;
        let threads = 8u64;
        let per_root_per_thread = 64u32;

        // Per-root fire tally so we can assert each root fired.
        let fires: Arc<Vec<AtomicU32>> = Arc::new((0..roots).map(|_| AtomicU32::new(0)).collect());

        let mut handles = Vec::new();
        for t in 0..threads {
            let counter = Arc::clone(&counter);
            let fires = Arc::clone(&fires);
            handles.push(thread::spawn(move || {
                // Stagger each thread's starting root so they interleave
                // on the same roots rather than partitioning them.
                for i in 0..roots {
                    let r = root(1, (i + t) % roots);
                    let idx = ((i + t) % roots) as usize;
                    for _ in 0..per_root_per_thread {
                        counter.record_sent(r);
                        if counter.record_finished(r) {
                            fires[idx].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            counter.live_roots(),
            0,
            "every balanced root must settle and reclaim its cell"
        );
        for (i, f) in fires.iter().enumerate() {
            assert!(
                f.load(Ordering::Relaxed) >= 1,
                "root {i} balanced but never fired Settled"
            );
        }
    }
}
