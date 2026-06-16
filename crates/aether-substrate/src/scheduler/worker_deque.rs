//! Per-worker work-stealing deque (ADR-0087 Phase 3a, iamacoffeepot/aether#1112).
//!
//! Each pool worker owns a `crossbeam_deque::Worker` deque, held in a
//! thread-local so both the worker's own loop (pop / steal-into) and the
//! inbox-sender wake path (push) — which run on the same thread when a
//! handler wakes a downstream slot — reach it without threading a
//! reference through every call site. Sibling workers hold `Stealer`s and
//! an off-worker [`Injector`] feeds producers with no worker thread.
//!
//! This supersedes the issue-1059 single-cell affinity stash: the deque's
//! **LIFO own-pop is the same warm-chain locality** the cell provided, so
//! a relay chain stays on one warm worker with no shared-queue round-trip
//! and no parked-sibling wake (~4.3µs). By default a worker **inlines its
//! local cascade** ([`try_push_local_budgeted`], iamacoffeepot/aether#1174):
//! every blob a running handler produces is a descendant of the cascade
//! already on this worker, so keeping it warm costs no cross-worker handoff
//! at *any* generation. Inlining holds until the per-burst **time valve**
//! ([`time_budget`]) trips — then the backlog spills so a *heavy* cascade
//! parallelises across idle workers. The budget is **adaptive**
//! (iamacoffeepot/aether#1182): a small multiple of the measured
//! cross-worker handoff cost ([`handoff_cost`]) — the
//! thing the valve out-amortises — so it tracks the hardware instead of a
//! one-box constant (the prior fixed 12µs sat at `≈ 6 ×` this box's ~2µs
//! handoff). `AETHER_LOCAL_TIME_BUDGET_US` still overrides. Duration is the
//! discriminator: a cheap cascade's whole burst stays fully inlined (no
//! bimodal), while a heavy one trips the valve and spills
//! (iamacoffeepot/aether#1174 matrix: heavy −15% end-to-end, trivial flat).
//! `AETHER_LOCAL_TIME_BUDGET_US=0` disables the valve (pure inline-cascade,
//! bounded only by the deque-length backstop `AETHER_LOCAL_STICKY_MAX`,
//! [`hard_cap`]). A worker is also **owner-only** over its own deque by
//! default ([`peer_steal_enabled`], iamacoffeepot/aether#1174): it pulls only
//! the shared injector, never a sibling's cascade. Set `AETHER_PEER_STEAL=1`
//! to opt the sibling-tail raid back in.
//!
//! The depth-0 keep-local exemption (the `len > 0` term in
//! [`try_push_local_budgeted`]'s spill condition) means a serial relay
//! chain — own deque oscillating 0→1→0 — never reads the clock, never
//! trips the time valve, and never visits the injector. A
//! *self-sustaining* chain (A mails B, B mails A, no external pacing)
//! would therefore monopolise its worker indefinitely and, with one such
//! loop per worker, starve the injector completely. The **every-K chain
//! backstop** (iamacoffeepot/aether#1535) bounds that monopoly: every
//! [`chain_backstop`] consecutive own-deque pops ([`chain_pop_due`],
//! `AETHER_LOCAL_CHAIN_BACKSTOP`, default 64) `acquire_slot` takes one
//! look at the injector before continuing the chain, so injector
//! starvation is bounded at ~K × cycle-time per worker while the chain
//! stays warm K−1 of K cycles — no clock read added.
//!
//! Only pool-worker threads call [`install`]; on any other thread
//! (chassis main, the hub, the trace drainer) [`try_push_local_budgeted`]
//! is a no-op spill and [`pop_local`] / [`steal_into_local`] yield nothing.

use std::cell::{Cell, RefCell};
use std::env;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};

use crate::config::{KnobKind, KnobRecord};
use crate::scheduler::calibrate::handoff_cost;
use crate::scheduler::slot::Drainable;

/// Discovery records for the four deque / keep-local-valve hot-path
/// tuning knobs (ADR-0090 unit b2, iamacoffeepot/aether#1255). These
/// describe the `OnceLock` getters below ([`hard_cap`], [`time_budget`],
/// [`peer_steal_enabled`], [`chain_backstop`]) so e1's
/// unknown-`AETHER_*`
/// sweep doesn't flag them and e2's `--config` dump lists them. The
/// hot-path read stays exactly as-is — these are pure `&'static`
/// metadata assembled once at boot, never on the dispatch path. Docs +
/// defaults are lifted verbatim from the getter doc-comments;
/// `time_budget` is adaptive with no literal default, so its `default`
/// is `None` (rendered "derived/unset").
pub const DEQUE_KNOBS: &[KnobRecord] = &[
    KnobRecord {
        env_key: "AETHER_LOCAL_STICKY_MAX",
        doc: "Deque-length backstop: max slots a worker keeps on its own deque \
              before forcing a spill (values < 1 / unparseable fall back).",
        default: Some("256"),
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_LOCAL_TIME_BUDGET_US",
        doc: "Keep-local time valve (microseconds): pins/disables the burst spill \
              valve. Unset → adaptive, derived from the measured handoff cost; \
              0 disables the valve (pure inline-cascade).",
        default: None,
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_PEER_STEAL",
        doc: "Whether idle workers may raid siblings' deques (peer-deque stealing). \
              Default off (owner-only); set 1/true to opt the sibling raid back in.",
        default: Some("off"),
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_LOCAL_CHAIN_BACKSTOP",
        doc: "Every-K injector backstop for keep-local chains: after K consecutive \
              own-deque pops a worker probes the injector once before continuing \
              its chain, bounding injector starvation at ~K cycles per worker \
              (values < 1 / unparseable fall back).",
        default: Some("64"),
        kind: KnobKind::HandRegistered,
    },
];

/// The unit on the deques: a chassis-registered dispatcher slot. (Phase
/// 3b makes the blob the unit; 3a keeps the slot.)
type Slot = Arc<dyn Drainable>;

thread_local! {
    /// This worker's own deque. `Some` only on a pool-worker thread
    /// (set by [`install`] at the top of the worker loop). `RefCell`
    /// because both the worker loop and a nested handler wake touch it
    /// on the same thread — never across a `run_cycle`, so the borrows
    /// don't overlap.
    static LOCAL: RefCell<Option<Worker<Slot>>> = const { RefCell::new(None) };

    /// The shared off-worker [`Injector`], registered per worker by
    /// [`install_injector`] at the top of the worker loop
    /// (iamacoffeepot/aether#1134). Held only so [`pending_depth`] can read
    /// the injector backlog without threading a reference through the
    /// deposit path; `None` on non-worker threads (chassis main, hub,
    /// off-worker injects), where `pending_depth` reports `0`.
    static INJECTOR: RefCell<Option<Arc<Injector<Slot>>>> = const { RefCell::new(None) };

    /// Start instant of the current burst, anchored on its first mail by
    /// [`burst_note_mail`] when time budgeting is on (iamacoffeepot/aether#1160).
    /// `None` when time budgeting is off (the default — no clock ever read)
    /// or before the burst's first mail. [`burst_over_budget`] reads
    /// `elapsed()` against it at decision time.
    static BURST_START: Cell<Option<Instant>> = const { Cell::new(None) };

    /// Consecutive own-deque `pop_local` hits since the last injector
    /// probe or burst reset — the every-K chain-backstop counter
    /// (iamacoffeepot/aether#1535). [`chain_pop_due`] bumps it per pop hit
    /// and reports `true` (resetting) every [`chain_backstop`]-th hit;
    /// [`burst_reset`] zeroes it when the own deque drains empty (the
    /// chain is over — the next cascade starts fresh). Single-writer
    /// (only the running worker touches it, never across a `run_cycle`),
    /// so a plain `Cell` — no atomics.
    static CHAIN_POPS: Cell<u32> = const { Cell::new(0) };
}

/// Move this worker's deque into its thread-local. Called once at the top
/// of the worker loop; enables local push/pop on this thread.
pub fn install(worker: Worker<Slot>) {
    LOCAL.with(|w| *w.borrow_mut() = Some(worker));
}

/// Register the shared injector for this worker thread so
/// [`pending_depth`] can read its backlog (iamacoffeepot/aether#1134).
/// Called once alongside [`install`] at the top of the worker loop;
/// no-op effect on dispatch (depth is measurement-only).
pub fn install_injector(injector: Arc<Injector<Slot>>) {
    INJECTOR.with(|i| *i.borrow_mut() = Some(injector));
}

/// Scheduler ready-queue depth observed from this thread: this worker's
/// own-deque len plus the shared injector len (iamacoffeepot/aether#1134).
/// `0` off any pool worker (no own deque installed) — chassis-root
/// injects and other off-worker deposits report no backlog. Read at mail
/// deposit and carried on the envelope so the latency harness can split
/// queue residence into *wakeup* (depth 0) vs *wait-behind-N* (load).
///
/// Both `Worker::len` and `Injector::len` are cheap O(1)-ish reads; this
/// is a relaxed snapshot, not a synchronization point — a racing push by
/// a sibling may land just after the read, which is fine for a profiling
/// signal.
#[must_use]
pub fn pending_depth() -> u32 {
    let own = LOCAL.with(|w| w.borrow().as_ref().map_or(0, Worker::len));
    let injected = INJECTOR.with(|i| i.borrow().as_ref().map_or(0, |inj| inj.len()));
    u32::try_from(own.saturating_add(injected)).unwrap_or(u32::MAX)
}

/// Deque-length backstop (iamacoffeepot/aether#1160, #1174) — the max slots a
/// worker keeps on its own deque before [`try_push_local_budgeted`] is forced
/// to spill, so a pathological unbounded local cascade can't grow the deque
/// without bound. Read once from `AETHER_LOCAL_STICKY_MAX` (repurposed from
/// the pre-#1160 stickiness cap); values `< 1` and unparseable input fall
/// back to `256`. This is the deque-growth backstop, not the primary
/// governor — the per-burst time valve ([`time_budget`], default 12µs) is;
/// for any realistic cascade (well under 256 blobs queued at once) `hard_cap`
/// never trips.
#[must_use]
// Process-level scheduler tuning knob (deque-growth backstop), read once at the
// substrate level — not cap config.
#[allow(clippy::disallowed_methods)]
pub fn hard_cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        env::var("AETHER_LOCAL_STICKY_MAX")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&k| k >= 1)
            .unwrap_or(256)
    })
}

/// Adaptive keep-local budget: spend up to this many measured cross-worker
/// **handoffs**' worth of time inlining a cascade before the valve spills
/// (iamacoffeepot/aether#1182). The valve out-amortises the cost of *not*
/// inlining — handing a blob to a parked sibling — so the budget should be
/// a small multiple of that handoff cost, not a fixed wall-clock figure.
///
/// `6` reproduces the #1174-tuned default on the box it was tuned on: that
/// 12µs sits at `≈ 6 ×` this box's measured ~2µs handoff
/// ([`handoff_cost`]). On a slower box (a more expensive
/// handoff) the budget scales up — more inlining is worth it before paying
/// the steeper handoff — and on a faster box it scales down, so the
/// trivial-vs-heavy discrimination tracks the hardware instead of a
/// one-box constant.
const BUDGET_HANDOFF_MULTIPLIER: u32 = 6;

/// Safety rails on the adaptive budget. These guard a *pathological*
/// handoff measurement (a sub-µs read on a quiet probe, or an absurd
/// outlier) from producing a nonsensical valve — they are not the
/// operating point, which `BUDGET_HANDOFF_MULTIPLIER × handoff_cost` sets
/// and which lands comfortably inside the rails on every real box measured
/// so far. The floor stays at the lowest budget the #1174 matrix ever
/// exercised; the ceiling caps a very slow box at a still-reasonable burst.
const MIN_ADAPTIVE_BUDGET: Duration = Duration::from_micros(6);
const MAX_ADAPTIVE_BUDGET: Duration = Duration::from_micros(60);

/// Keep-local **time** budget per burst — the spill valve, **on by
/// default** (iamacoffeepot/aether#1160, #1174; made adaptive in #1182). A
/// worker inlines its whole cascade until the burst has run this long, then
/// spills the backlog so a heavy cascade parallelises across idle workers.
/// Duration is the discriminator (it separates a cheap cascade from a heavy
/// one where mail-count can't): a trivial tree's whole burst stays inlined,
/// while a heavy cascade trips the valve and spills.
///
/// `AETHER_LOCAL_TIME_BUDGET_US` (microseconds) overrides — an explicit
/// value pins the budget and `0` disables the valve (pure inline-cascade,
/// bounded only by `hard_cap`). Unset (the default), the budget is
/// **derived from the measured handoff cost**: `derive_budget` of
/// [`handoff_cost`] — `BUDGET_HANDOFF_MULTIPLIER ×` the
/// boot-probed, live-refined cross-worker handoff on this box, clamped to
/// the safety rails. Reading the live estimate (rather than a boot
/// snapshot) keeps the budget tracking the *operating* handoff cost the
/// valve actually has to out-amortise; the read is a couple of relaxed
/// atomic loads, negligible against the dispatch it gates. The wall clock
/// is still sampled at decision time, not per mail (#1163).
#[must_use]
// Process-level scheduler tuning knob (keep-local time-budget override), read once
// at the substrate level — not cap config.
#[allow(clippy::disallowed_methods)]
pub fn time_budget() -> Duration {
    // An explicit env budget wins and is fixed within a run — pin or
    // disable (0) the valve regardless of the measured handoff cost. Cached
    // because the override never changes.
    static OVERRIDE_US: OnceLock<Option<u64>> = OnceLock::new();
    if let Some(us) = *OVERRIDE_US.get_or_init(|| {
        env::var("AETHER_LOCAL_TIME_BUDGET_US")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    }) {
        return Duration::from_micros(us);
    }
    derive_budget(handoff_cost())
}

/// The adaptive budget for a given handoff cost: `BUDGET_HANDOFF_MULTIPLIER
/// ×` it, clamped to the safety rails. Split out from the env read so the
/// derivation is unit-testable without touching the process-global
/// estimate.
fn derive_budget(handoff: Duration) -> Duration {
    handoff
        .saturating_mul(BUDGET_HANDOFF_MULTIPLIER)
        .clamp(MIN_ADAPTIVE_BUDGET, MAX_ADAPTIVE_BUDGET)
}

/// Whether idle workers may raid siblings' deques (peer-deque stealing),
/// read once from `AETHER_PEER_STEAL`; default **off** — each worker is
/// **owner-only** over its own deque (iamacoffeepot/aether#1174). Set
/// **1** (or `true`) to opt the sibling raid back in.
///
/// The default flipped to owner-only because peer-deque stealing stopped
/// being load-bearing after seize-direct (iamacoffeepot/aether#1135),
/// cursor-shared cooperative blob (iamacoffeepot/aether#1141), and the
/// keep-local budget (iamacoffeepot/aether#1160): a blob on a worker's own
/// deque is there *because the budget judged it cheap* — it didn't spill.
/// Raiding it pays a cache-cold cross-worker handoff for sub-threshold work,
/// so the steal can cost more than the work is worth, and it contradicts the
/// decision that kept the blob local. Worthwhile (wide / heavy) work
/// parallelises through the injector via spill + recruit, which the
/// unconditional injector drain still serves. The cost of owner-only is the
/// loss of the budget-misclassification safety net — heavy work the budget
/// *wrongly* keeps local strands on its owner with no idle-sibling rescue,
/// raising the stakes on the iamacoffeepot/aether#1128 cost classification;
/// `AETHER_PEER_STEAL=1` restores the rescue.
#[must_use]
// Process-level scheduler tuning knob (peer-deque-steal opt-in), read once at the
// substrate level — not cap config.
#[allow(clippy::disallowed_methods)]
pub fn peer_steal_enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| {
        env::var("AETHER_PEER_STEAL")
            .ok()
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    })
}

/// Every-K chain backstop (iamacoffeepot/aether#1535) — how many
/// consecutive own-deque pops a worker takes before [`chain_pop_due`]
/// directs `acquire_slot` to probe the injector once. The depth-0
/// keep-local exemption deliberately keeps a serial chain warm with no
/// clock read (iamacoffeepot/aether#1174), so a *self-sustaining* chain
/// would otherwise monopolise its worker forever; this bounds injector
/// starvation at ~K × cycle-time per worker while the chain stays warm
/// K−1 of K cycles. Read once from `AETHER_LOCAL_CHAIN_BACKSTOP`; values
/// `< 1` and unparseable input fall back to `64`.
#[must_use]
// Process-level scheduler tuning knob (every-K chain backstop), read once at the
// substrate level — not cap config.
#[allow(clippy::disallowed_methods)]
pub fn chain_backstop() -> u32 {
    static K: OnceLock<u32> = OnceLock::new();
    *K.get_or_init(|| {
        env::var("AETHER_LOCAL_CHAIN_BACKSTOP")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&k| k >= 1)
            .unwrap_or(64)
    })
}

/// Note one own-deque `pop_local` hit against the every-K chain backstop
/// (iamacoffeepot/aether#1535): bump the consecutive-pop counter and
/// report `true` — resetting it — on every [`chain_backstop`]-th hit.
/// `acquire_slot` calls this on its pop-hit path and, when due, runs one
/// `steal_into_local` pass before continuing the chain. The counter also
/// resets in [`burst_reset`] (own deque drained empty — the chain is
/// over). A `Cell` bump and an integer compare per pop; no clock read.
#[must_use]
pub fn chain_pop_due() -> bool {
    let n = CHAIN_POPS.get().saturating_add(1);
    if n >= chain_backstop() {
        CHAIN_POPS.set(0);
        true
    } else {
        CHAIN_POPS.set(n);
        false
    }
}

/// Note one dispatched envelope against the current local-drain burst
/// (iamacoffeepot/aether#1160). When time budgeting is on
/// (`time_budget > 0`), anchors the burst start on the first mail so
/// `burst_over_budget` can measure elapsed at decision time. With
/// `time_budget == 0` (the valve disabled) the clock is never read — a
/// no-op. The clock is sampled at *decision* time, not per mail
/// (iamacoffeepot/aether#1160 fix): the prior strided per-mail sample never
/// fired for a sub-stride burst, so a narrow *heavy* cascade (few mail ×
/// expensive handlers) never tripped the time budget — exactly the case the
/// valve exists to catch.
pub fn burst_note_mail(time_budget: Duration) {
    // Anchor the burst start at its first mail (one clock read per burst,
    // only when time budgeting is on) so a heavy first handler's elapsed is
    // counted by the time path — not deferred to the first decision, which
    // would under-count the work that already ran. The burst's first mail is
    // the one that finds no start anchored yet.
    if !time_budget.is_zero() && BURST_START.get().is_none() {
        BURST_START.set(Some(Instant::now()));
    }
}

/// Has the current burst exceeded its keep-local **time** budget
/// (iamacoffeepot/aether#1160, #1174)? `time_budget` (default 12µs; `0`
/// disables): `true` once the burst has run past `time_budget` since its
/// first mail — the discriminator that spills heavy cascades but leaves
/// cheap ones inlined. The wall clock is read only when time budgeting is
/// on — once per genuine keep-vs-spill decision on a multi-blob backlog.
/// Called by [`try_push_local_budgeted`] only after the `depth > 0` guard,
/// so a single-blob fan-out or a chain (depth 0) reads no clock at all.
#[must_use]
pub fn burst_over_budget(time_budget: Duration) -> bool {
    if time_budget.is_zero() {
        return false;
    }
    BURST_START
        .get()
        .is_some_and(|start| start.elapsed() >= time_budget)
}

/// Reset the local-drain burst counters (iamacoffeepot/aether#1160) and the
/// chain-backstop pop counter (iamacoffeepot/aether#1535). Called by
/// `acquire_slot` the moment `pop_local` reports the own deque drained
/// empty, so each local cascade is one burst and any subsequently stolen
/// work starts a fresh budget — and the next chain starts a fresh
/// every-K count.
pub fn burst_reset() {
    BURST_START.set(None);
    CHAIN_POPS.set(0);
}

/// Push of a just-produced blob onto this worker's own deque
/// (iamacoffeepot/aether#1160, #1174). Every blob this sees was produced by a
/// handler running on this worker — a **descendant of the cascade already on
/// this worker** — so it is kept local (inlined, warm) until the burst trips
/// the time valve or the deque-length backstop:
///
/// ```text
/// spill  ⟺  local_deque_len >= hard_cap
///           || (local_deque_len > 0 && burst_over_budget(time_budget))
/// ```
///
/// By default `time_budget` is 12µs, so [`burst_over_budget`] is the
/// **time valve**: a cheap cascade (whole burst ≈ 6µs) stays fully inlined
/// at every generation — no bimodal, no cross-worker wakeup for sub-threshold
/// work — while a heavy cascade trips the valve after ~12µs and spills its
/// backlog to parallelise (iamacoffeepot/aether#1174 matrix: heavy −15%
/// end-to-end, trivial flat). The `len > 0` guard keeps a serial chain or
/// single-blob fan-out local with no clock read. Set
/// `AETHER_LOCAL_TIME_BUDGET_US=0` to disable the valve (pure inline-cascade).
///
/// Returns `Ok(())` when kept local (the caller skips injector + notify),
/// or `Err(slot)` to spill. Off a pool worker there is no own deque, so
/// always `Err` (spill).
pub fn try_push_local_budgeted(
    slot: Slot,
    time_budget: Duration,
    hard_cap: usize,
) -> Result<(), Slot> {
    LOCAL.with(|w| {
        let w = w.borrow();
        match w.as_ref() {
            Some(worker) => {
                let len = worker.len();
                let spill = len >= hard_cap || (len > 0 && burst_over_budget(time_budget));
                if spill {
                    Err(slot)
                } else {
                    worker.push(slot);
                    Ok(())
                }
            }
            None => Err(slot),
        }
    })
}

/// Pop this worker's next own-deque slot (LIFO — most-recently-pushed,
/// i.e. the freshest relay hop, stays warmest). Checked before stealing
/// and before the park, so an own slot is never stranded.
pub fn pop_local() -> Option<Slot> {
    LOCAL.with(|w| w.borrow().as_ref().and_then(Worker::pop))
}

/// Unconditional push back onto this worker's own deque — the re-queue
/// arm of the every-K chain backstop (iamacoffeepot/aether#1535).
/// `acquire_slot` gives a just-popped chain slot back when the backstop
/// probe found injector work to run first: the stolen slot runs now and
/// the chain slot waits at the LIFO top (it is the next pop, so the
/// chain resumes right after). No budget terms — the keep-local decision
/// was already made when the slot was pushed; this only restores it. Off
/// a pool worker there is no own deque, so the slot is handed back
/// (`Err`) for the caller to spill.
pub fn push_local(slot: Slot) -> Result<(), Slot> {
    LOCAL.with(|w| match w.borrow().as_ref() {
        Some(worker) => {
            worker.push(slot);
            Ok(())
        }
        None => Err(slot),
    })
}

/// Steal work into this worker's own deque and return one slot to run.
/// Prefers the [`Injector`] (off-worker producers + spilled fan-out +
/// requeued yields, so external work isn't starved by sibling stealing),
/// then — only when `peer_steal` is set — each sibling's [`Stealer`]
/// (skipping our own `my_idx`). Returns `None` when every consulted source
/// is empty. Non-blocking — safe as the `SpinPark::acquire` scan closure
/// (its spin loop + park-commit recheck call it repeatedly).
///
/// `peer_steal` (read once per worker from [`peer_steal_enabled`]) gates the
/// sibling raid only — the injector drain is unconditional and load-bearing.
/// With it off, this worker is **owner-only** over its deque
/// (iamacoffeepot/aether#1174): it never pulls a sibling's keep-local
/// cascade, so a cheap blob the budget kept local isn't dragged
/// cross-worker.
pub fn steal_into_local(
    my_idx: usize,
    stealers: &[Stealer<Slot>],
    injector: &Injector<Slot>,
    peer_steal: bool,
) -> Option<Slot> {
    LOCAL.with(|w| {
        let w = w.borrow();
        let worker = w.as_ref()?;
        // Retry the whole pass while any source reports transient
        // contention; return on the first success; `None` once all empty.
        loop {
            let mut retry = false;
            match injector.steal_batch_and_pop(worker) {
                Steal::Success(slot) => return Some(slot),
                Steal::Retry => retry = true,
                Steal::Empty => {}
            }
            if peer_steal {
                for (i, stealer) in stealers.iter().enumerate() {
                    if i != my_idx {
                        match stealer.steal_batch_and_pop(worker) {
                            Steal::Success(slot) => return Some(slot),
                            Steal::Retry => retry = true,
                            Steal::Empty => {}
                        }
                    }
                }
            }
            if !retry {
                return None;
            }
        }
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: a failed steal/pop assertion is the test signal"
)]
mod tests {
    use super::*;
    use crate::scheduler::SpinPark;
    use crate::scheduler::slot::{BatchBudget, CycleResult, Drainable, WakeSink};
    use std::any::Any;
    use std::thread;

    struct Noop;
    impl Drainable for Noop {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn noop() -> Slot {
        Arc::new(Noop)
    }

    /// Drain any residue so the per-thread deque starts empty regardless
    /// of test scheduling order on a shared thread.
    fn drain_local() {
        while pop_local().is_some() {}
    }

    #[test]
    fn derive_budget_reproduces_the_tuned_default_on_this_box() {
        // The whole point of k = 6: a ~2µs handoff (this box's measured
        // cost) derives the #1174-tuned 12µs budget, so wiring the valve to
        // the measurement is behaviour-preserving where it was tuned.
        assert_eq!(
            derive_budget(Duration::from_micros(2)),
            Duration::from_micros(12),
        );
    }

    #[test]
    fn derive_budget_scales_with_handoff_cost() {
        // A slower box (more expensive handoff) gets a larger budget — more
        // inlining is worth it before paying the steeper handoff.
        assert_eq!(
            derive_budget(Duration::from_micros(5)),
            Duration::from_micros(30),
        );
    }

    #[test]
    fn derive_budget_clamps_to_safety_rails() {
        // A pathologically small measurement floors at the rail rather than
        // producing a sub-µs valve that spills everything…
        assert_eq!(
            derive_budget(Duration::from_nanos(100)),
            MIN_ADAPTIVE_BUDGET,
            "6 × 100ns = 600ns clamps up to the floor",
        );
        // …and an absurd outlier caps at the ceiling rather than inlining
        // for a wildly long burst.
        assert_eq!(
            derive_budget(Duration::from_micros(50)),
            MAX_ADAPTIVE_BUDGET,
            "6 × 50µs = 300µs clamps down to the ceiling",
        );
    }

    #[test]
    fn budgeted_off_worker_always_spills() {
        // This test never calls `install`, so it isn't a pool worker:
        // every push must spill regardless of budget or backlog.
        assert!(try_push_local_budgeted(noop(), Duration::ZERO, 256).is_err());
        assert!(pop_local().is_none());
    }

    #[test]
    fn inline_cascade_valve_off_keeps_local_past_budget() {
        // #1174: with the valve off (`time_budget == 0`) a worker inlines its
        // ENTIRE cascade — every blob is kept local even when the burst is
        // long-running, because no spill term fires at any generation. Only
        // `hard_cap` bounds it. (The shipped default leaves the time valve on
        // at 12µs; this is the `AETHER_LOCAL_TIME_BUDGET_US=0` pure-inline
        // opt-out.)
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        for _ in 0..100 {
            burst_note_mail(Duration::ZERO); // valve off ⇒ no clock anchored
        }
        // Time off ⇒ `burst_over_budget` is always false, so the descendant
        // is kept local regardless of depth or burst length.
        assert!(try_push_local_budgeted(noop(), Duration::ZERO, 256).is_ok());
        assert!(
            try_push_local_budgeted(noop(), Duration::ZERO, 256).is_ok(),
            "valve off keeps a descendant local at depth > 0, over any burst"
        );
        // The deque-length backstop still bounds it: with hard_cap 2, the
        // third push (len == 2) spills even with the valve off.
        assert!(
            try_push_local_budgeted(noop(), Duration::ZERO, 2).is_err(),
            "hard_cap still bounds the inline cascade"
        );
        drain_local();
    }

    #[test]
    fn budgeted_chain_never_spills_at_depth_zero() {
        // The load-bearing guard: a serial chain has an empty deque at
        // schedule time (the current blob was popped), so it stays local
        // even when the burst is well over the time budget — a chain has no
        // independent work to parallelize, so a spill would only buy a
        // wakeup.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        let tiny = Duration::from_nanos(1);
        burst_note_mail(tiny); // anchors the burst start
        thread::sleep(Duration::from_micros(50)); // now well past `tiny`
        assert!(
            burst_over_budget(tiny),
            "burst should read over the tiny time budget"
        );
        assert!(
            try_push_local_budgeted(noop(), tiny, 256).is_ok(),
            "depth 0 keeps local even over budget (the chain guard)"
        );
        drain_local();
    }

    #[test]
    fn budgeted_keeps_local_under_budget() {
        // Under the time budget (a generous valve, a fresh burst) a cascade
        // stacks on the own deque — the keep-local win the spill cost avoids.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        let generous = Duration::from_secs(10);
        for _ in 0..5 {
            burst_note_mail(generous); // anchors the burst start once
            assert!(
                try_push_local_budgeted(noop(), generous, 256).is_ok(),
                "under budget keeps local"
            );
        }
        drain_local();
    }

    #[test]
    fn budgeted_hard_cap_backstop() {
        // Even under the time budget, the deque-length backstop forces a
        // spill once the own deque reaches `hard_cap`.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        // hard_cap 2, valve off (never trips by time).
        assert!(try_push_local_budgeted(noop(), Duration::ZERO, 2).is_ok()); // len 0 → 1
        assert!(try_push_local_budgeted(noop(), Duration::ZERO, 2).is_ok()); // len 1 → 2
        assert!(
            try_push_local_budgeted(noop(), Duration::ZERO, 2).is_err(),
            "len == hard_cap spills regardless of budget"
        );
        drain_local();
    }

    #[test]
    fn budgeted_time_valve_spills_few_mail_heavy() {
        // The core #1160 fix: a *few-mail* burst that has run past the time
        // budget spills its backlog (depth > 0) — the narrow-heavy case the
        // strided per-mail clock used to miss (it never sampled a sub-stride
        // burst).
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        let tiny = Duration::from_nanos(1);
        burst_note_mail(tiny); // anchors the burst start
        thread::sleep(Duration::from_micros(50)); // now well past `tiny`
        // Depth 0 still keeps — the chain guard short-circuits before the
        // budget (and before any clock read).
        assert!(
            try_push_local_budgeted(noop(), tiny, 256).is_ok(),
            "depth 0 keeps regardless of elapsed time"
        );
        // Depth 1, over the time budget → spill.
        assert!(
            try_push_local_budgeted(noop(), tiny, 256).is_err(),
            "over the time budget spills the backlog (the valve fix)"
        );
        drain_local();
    }

    #[test]
    fn burst_time_path_trips_over_budget() {
        // With time budgeting on, a burst that has run past the time budget
        // trips at decision time — the few-mail-heavy case the strided
        // per-mail sample used to miss (iamacoffeepot/aether#1160). The start
        // is anchored on the first mail; a tiny budget + a real sleep makes
        // the elapsed check at the decision deterministic.
        burst_reset();
        let tiny = Duration::from_nanos(1);
        burst_note_mail(tiny); // anchors BURST_START
        thread::sleep(Duration::from_micros(50));
        assert!(
            burst_over_budget(tiny),
            "the time path trips past the budget"
        );
        // Time budgeting off ⇒ the time path is never consulted, even though
        // the same elapsed time has passed.
        assert!(
            !burst_over_budget(Duration::ZERO),
            "time_budget 0 never trips on elapsed"
        );
        burst_reset();
        assert!(!burst_over_budget(tiny), "reset clears the burst start");
    }

    #[test]
    fn chain_pop_due_fires_every_k_and_resets() {
        // The every-K backstop counter (iamacoffeepot/aether#1535): quiet
        // for K−1 consecutive pop hits, due on the K-th, then the reset
        // makes the next window identical.
        burst_reset();
        let k = chain_backstop();
        for i in 1..k {
            assert!(!chain_pop_due(), "pop {i} of {k} must not be due");
        }
        assert!(chain_pop_due(), "the K-th consecutive pop is due");
        for i in 1..k {
            assert!(!chain_pop_due(), "post-reset pop {i} must not be due");
        }
        assert!(chain_pop_due(), "the window repeats after the due-reset");
    }

    #[test]
    fn burst_reset_clears_chain_pops() {
        // A drained deque ends the chain: `burst_reset` zeroes the
        // counter, so the next chain gets a full fresh window rather
        // than inheriting the old chain's progress toward K.
        burst_reset();
        let k = chain_backstop();
        for _ in 1..k {
            assert!(!chain_pop_due());
        }
        burst_reset(); // own deque drained — chain over
        for i in 1..k {
            assert!(
                !chain_pop_due(),
                "pop {i} after reset must restart the count, not inherit it"
            );
        }
        assert!(chain_pop_due());
    }

    #[test]
    fn push_local_off_worker_hands_back() {
        // No `install` on this thread — not a pool worker, so there is
        // no own deque to restore the slot to.
        assert!(push_local(noop()).is_err());
    }

    #[test]
    fn push_local_on_worker_requeues_unconditionally() {
        // The re-queue arm ignores every budget term: deque depth and
        // burst state don't matter, the slot was already keep-local.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        for _ in 0..100 {
            burst_note_mail(Duration::ZERO); // far past any budget
        }
        assert!(push_local(noop()).is_ok());
        assert!(push_local(noop()).is_ok(), "no budget term applies");
        assert!(pop_local().is_some());
        assert!(pop_local().is_some());
        assert!(pop_local().is_none());
        burst_reset();
    }

    #[test]
    fn schedule_default_keeps_local_on_worker() {
        // Drive the wired decision through `WakeSink::schedule` on a
        // simulated pool worker (own deque installed on this thread). Under
        // the Phase 3 keep-local default a small cascade (burst well under
        // budget) stays on the own deque — no spill, no sibling wakeup.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();

        let injector = Arc::new(Injector::<Slot>::new());
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()), 8);

        sink.schedule(noop()); // empty deque → kept local
        sink.schedule(noop()); // depth 1, burst under budget → still kept local

        assert!(
            matches!(injector.steal(), Steal::Empty),
            "under the keep-local default nothing spills to the injector"
        );
        assert!(
            pop_local().is_some(),
            "both schedules stay on the local deque"
        );
        assert!(pop_local().is_some());
        assert!(pop_local().is_none());
    }

    #[test]
    fn steal_pulls_from_injector_and_siblings() {
        install(Worker::new_lifo());
        drain_local();

        // Injector work is pulled.
        let injector: Injector<Slot> = Injector::new();
        injector.push(noop());
        assert!(steal_into_local(0, &[], &injector, true).is_some());

        // A sibling's deque is stolen from (own index 0 is skipped).
        let sibling: Worker<Slot> = Worker::new_lifo();
        sibling.push(noop());
        sibling.push(noop());
        let stealers = [Worker::<Slot>::new_lifo().stealer(), sibling.stealer()];
        assert!(
            steal_into_local(0, &stealers, &Injector::new(), true).is_some(),
            "should steal from sibling index 1"
        );

        // Nothing anywhere → None.
        drain_local();
        assert!(steal_into_local(0, &[], &Injector::new(), true).is_none());
    }

    #[test]
    fn steal_owner_only_skips_siblings() {
        // With `peer_steal == false` the injector drain stays load-bearing
        // but a sibling's deque is left untouched — owner-only
        // (iamacoffeepot/aether#1174).
        install(Worker::new_lifo());
        drain_local();

        // Injector is still drained regardless of peer_steal.
        let injector: Injector<Slot> = Injector::new();
        injector.push(noop());
        assert!(
            steal_into_local(0, &[], &injector, false).is_some(),
            "injector drain is load-bearing — unaffected by peer_steal"
        );

        // A sibling holds work; owner-only must NOT raid it.
        let sibling: Worker<Slot> = Worker::new_lifo();
        sibling.push(noop());
        let stealers = [Worker::<Slot>::new_lifo().stealer(), sibling.stealer()];
        assert!(
            steal_into_local(0, &stealers, &Injector::new(), false).is_none(),
            "owner-only leaves the sibling's keep-local cascade alone"
        );
        // The same sibling IS raided once peer steal is back on — proving the
        // flag is the only thing that changed (the work was there all along).
        assert!(
            steal_into_local(0, &stealers, &Injector::new(), true).is_some(),
            "peer_steal on raids the untouched sibling"
        );

        drain_local();
    }
}
