//! [`BlobWork`] — a producer's buffered fan-out as a single **cursor-shared
//! cooperative** work unit (ADR-0087, iamacoffeepot/aether#1137; builds on
//! the Phase 3b blob + the iamacoffeepot/aether#1135 claim-and-dispatch-
//! direct demux).
//!
//! A handler's outbound mail is grouped **by recipient** into one shared
//! blob and scheduled once. Many workers drain it cooperatively: each
//! claims a whole recipient-group off a shared cursor (the packed
//! [`Lifecycle`] word), seizes that recipient, and dispatches its mail
//! **in place** — the iamacoffeepot/aether#1135 fast path, now run by N
//! workers in parallel instead of one. A wide / heavy fan-out parallelises
//! across the pool instead of serialising on the demuxing worker
//! (iamacoffeepot/aether#1134 measured that serialisation; #1137 closes
//! it). Recruitment is a **broadcast** — the producer re-submits the blob
//! `Arc` to the shared injector + notify (`WakeSink::recruit`), so parked
//! siblings wake and race the cursor — not the own-deque spill the prior
//! 3c path used (which kept work local and never woke a sibling, so it
//! could not parallelise a fan-out at all).
//!
//! ## Recipient-grouped, each recipient once
//!
//! A worker owns a whole recipient-group, so **per-recipient FIFO is free**
//! (the ordering spine, ADR-0087 amendment): one worker dispatches all of a
//! recipient's mail, in send order. Cross-recipient groups run concurrently
//! — the spine makes that explicitly sound (different recipients → async,
//! no ordering guarantee). Each recipient appears in **at most one** group
//! per blob; successive flushes to the same recipient append to its
//! existing group (preserving cross-flush FIFO) rather than racing a second
//! group.
//!
//! ## Single active blob + append (cross-flush FIFO)
//!
//! [`BlobProducer`] keeps **one active blob per producing actor**.
//! Successive flushes append (new recipients → new groups via
//! [`Lifecycle::publish`]; seen recipients → push onto the existing group's
//! buffer). The blob retires when fully drained; the next flush rolls a
//! fresh one. Accumulation is what preserves per-recipient FIFO across
//! flushes under burst: a second flush to a not-yet-drained recipient lands
//! in the same group rather than a second group two workers could seize
//! out of order. A flush that overflows the group array (or hits a retired
//! blob) rolls the remainder into a fresh blob — and the overflow remainder
//! is always *new* recipients (seen recipients push to existing groups, no
//! new-group pressure), so a rolled blob never shares a recipient with the
//! one it rolled from: no cross-blob same-recipient race.
//!
//! ## Closeable per-group buffer (the merge-vs-claim handshake)
//!
//! Each group's mail lives behind a closeable buffer guarded by a one-byte
//! atomic-flag spinlock (`Group`). This is **SPSC**: the producing actor's
//! thread pushes; exactly one cursor-winning worker drains+closes. The
//! worker drains in a loop — taking whatever the buffer holds, dispatching
//! it, then re-locking — and **closes only when it locks and finds the
//! buffer empty**. That makes `close` a FIFO barrier: every mail the
//! producer pushed before the close is captured and dispatched (in order);
//! a push that loses the race sees `closed` and is deposited through
//! `route_mail`, landing in the recipient inbox strictly *after* everything
//! the worker dispatched. So a late cross-flush append never jumps ahead of
//! earlier mail. The atomic flag (not a `std::sync::Mutex`) avoids the
//! per-group `pthread_mutex_t` create-on-first-lock + destroy-on-drop churn
//! that the #1140 profile found dominant on macOS — a fresh group per flush
//! means a fresh OS mutex object per flush. Contention is rare by
//! construction (one producer, one worker), so the spin almost never loops;
//! a lock-free Treiber stack would reverse send order (LIFO), so it is
//! *not* a drop-in for the FIFO barrier.
//!
//! ## Reusing the one router (unchanged from #1135)
//!
//! In-place dispatch resolves the recipient's seize handle + ref-schema via
//! [`Registry::route_lookup`](crate::mail::Registry) and, on a won
//! `Idle → Running` seize of a ref-free kind, runs
//! [`Drainable::seize_and_run`] → `DispatcherSlot::dispatch_one` (the same
//! per-envelope wrapper a pooled `run_cycle` runs — `Received` / `Finished`
//! incl. the #1134 `t_enqueue` / `enqueue_depth`, the `record_finished`
//! settlement bracket, the `log.tail` / `trace.tail` arms). A busy slot
//! (lost seize), a non-`Pooled` recipient (no seize handle), or an ADR-0045
//! ref kind falls back to [`Mailer::push`] → `route_mail`, inheriting that
//! path's ref-walk / park / settlement / trace unchanged.
//!
//! ## Recruitment gate (cost-aware, iamacoffeepot/aether#1178)
//!
//! Recruiting siblings for a *cheap* fan-out would regress the
//! iamacoffeepot/aether#1116 narrow-local win (needless wakeups for cheap
//! handlers). The original gate used **width** as a proxy — broadcast only
//! when the fresh-group count cleared `AETHER_BLOB_RECRUIT_MIN` — but width
//! cannot tell a heavy narrow fan-out (which benefits from parallelism)
//! from a trivial one (which does not), so a heavy `<= 8` fan-out stayed
//! serial and a single global threshold could not be right for two
//! components at once. iamacoffeepot/aether#1178 replaces width with a
//! **cost-aware** decision: at flush, sum iamacoffeepot/aether#1128's
//! per-handler execution-cost EWMA over the flush's fresh recipient groups
//! and size recruitment by the actual work. The recipient group is the unit
//! of parallelism (a worker drains a whole group; groups can't split), so
//! the blob is a parallel-machine makespan problem — optimal
//! `K ≈ ceil(total_work / longest_pole)`, clamped to `[1, min(G, W)]` and
//! gated by `total_work > C_WAKE_NANOS` (the wake break-even). [`recruit_k`]
//! is that closed form. A cheap narrow blob yields `K = 1` and stays local
//! — preserving the #1116 win for the right reason (cheapness, not
//! narrowness) — while a heavy narrow blob recruits its full group count.
//!
//! When any contributing handler's cell is **unknown cost** — a neutral
//! seed (`samples == 0`, known-but-never-run) or a high mean-abs-deviation
//! (bimodal, untrustworthy) — the cost estimate is noise, so the flush
//! falls back to the **width gate** (`AETHER_BLOB_RECRUIT_MIN`) for that
//! blob rather than recruiting on a bad number. Width is the confidence
//! fallback, no longer the primary signal.
//!
//! **Aggregation scope (iamacoffeepot/aether#1178 step 5):** `total_work` /
//! `max_group_work` are summed over **this flush's fresh groups**, matching
//! the existing per-flush `fresh_groups` width gate — numerator and
//! denominator share the same scope, the recruit broadcast still fires once
//! per flush, and a leftover-roll flush (0 fresh groups) recruits nobody.
//! A worker races the cursor over *all* published groups, but each flush's
//! recruit sizes the *newly-published* parallelizable work it is waking
//! siblings for.

use std::any::Any;
use std::cell::UnsafeCell;
use std::env;
use std::hint::spin_loop;
use std::mem;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use aether_kinds::trace::Nanos;
use rustc_hash::FxHashMap;

use crate::actor::native::Envelope;
use crate::actor::native::blob_lifecycle::{Lifecycle, MAX_GROUPS, Published};
use crate::mail::cost::CostLookup;
use crate::mail::mailer::Mailer;
use crate::mail::{KindId, Mail, MailboxId};
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SeizeHandle, WakeSink};

/// Floor for a fresh blob's group-array capacity — a little headroom so a
/// couple of subsequent flushes to *new* recipients can accumulate before
/// the array overflows and the producer rolls a fresh blob. Kept small: a
/// wide fan-out already sizes its array to its own width
/// ([`group_cap_for`]), so the floor only governs *narrow* flushes (a chain
/// hop, a tiny fan-out), where a large floor is pure wasted allocation —
/// those blobs almost always drain before any second flush appends.
const GROUP_CAP_MIN: usize = 4;

/// Minimum fresh-group count for a flush to broadcast-recruit siblings.
/// Read once from `AETHER_BLOB_RECRUIT_MIN`; values `< 1` and unparseable
/// input fall back to the default. **Default 9** keeps narrow `<= 8`
/// fan-outs on the producer-local inline path (the
/// iamacoffeepot/aether#1116 narrow-local win) and recruits only wider
/// fan-outs. See the module doc on the width-vs-cost proxy limitation
/// (iamacoffeepot/aether#1127).
fn recruit_min() -> usize {
    static MIN: OnceLock<usize> = OnceLock::new();
    *MIN.get_or_init(|| {
        env::var("AETHER_BLOB_RECRUIT_MIN")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&k| k >= 1)
            .unwrap_or(9)
    })
}

/// Cap on the number of sibling copies a single flush injects when
/// recruiting. Read once from `AETHER_BLOB_RECRUIT_MAX`; bounds the
/// injector churn for a very wide fan-out (over-recruiting past the worker
/// count just re-parks the extra workers — harmless but wasteful). Default
/// 32.
fn recruit_cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        env::var("AETHER_BLOB_RECRUIT_MAX")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&k| k >= 1)
            .unwrap_or(32)
    })
}

/// Wake break-even in nanos: the measured injector pickup / park-wake
/// floor (iamacoffeepot/aether#1174's ~7µs tree floor, attributed to the
/// injector handoff). A flush whose summed group work is below this can
/// never amortize a sibling wakeup, so [`recruit_k`] returns `K = 1` and
/// the blob stays producer-local — the cost-aware form of the prior
/// narrow-local win (iamacoffeepot/aether#1116). A runtime-measured
/// `C_wake` is a deferred upgrade (iamacoffeepot/aether#1127); for now it
/// is this const, overridable via `AETHER_WAKE_COST_NANOS`.
const C_WAKE_NANOS: u64 = 4_300;

/// High-MAD confidence threshold: a handler whose mean-absolute-deviation
/// exceeds this fraction of its mean is "bimodal / untrustworthy" — its
/// EWMA mean is a poor predictor of the next dispatch, so the recruiter
/// treats it as unknown cost and falls back to the width gate. Expressed
/// as a `>` ratio test (`mad * MAD_CONFIDENCE_DEN > mean *
/// MAD_CONFIDENCE_NUM`) to stay integer-only on the flush path. `1/1`
/// means "MAD as large as the mean" — a spread that doubles the estimate's
/// uncertainty; below that the mean is trustworthy enough to size on.
const MAD_CONFIDENCE_NUM: u64 = 1;
const MAD_CONFIDENCE_DEN: u64 = 1;

/// The wake break-even in nanos, honouring an `AETHER_WAKE_COST_NANOS`
/// override (unparseable / empty falls back to [`C_WAKE_NANOS`]). Read once.
fn wake_cost_nanos() -> u64 {
    static WAKE: OnceLock<u64> = OnceLock::new();
    *WAKE.get_or_init(|| {
        env::var("AETHER_WAKE_COST_NANOS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(C_WAKE_NANOS)
    })
}

/// The cost-aware recruit target `K` (iamacoffeepot/aether#1178): how many
/// workers (including the producer's own) should drain this flush's fresh
/// groups. The recipient group is the unit of parallelism (a worker drains
/// a whole group; groups can't split), so the blob is a parallel-machine
/// makespan problem and the optimum is `K ≈ ceil(total_work /
/// longest_pole)`, clamped to `[1, min(G, W)]`:
///
/// - `total_work` is `Σw` over the fresh groups (how *much* work there is),
/// - `max_group_work` is `w_max`, the longest pole (how *spreadable* it is
///   — no `K` can beat the single fattest group's serial drain),
/// - `g` is the fresh-group count `G` (can't use more workers than groups),
/// - `w` is the pool worker count `W` (can't use more workers than exist).
///
/// Below the wake break-even ([`wake_cost_nanos`]) the whole blob is too
/// cheap to amortize a sibling wakeup, so `K = 1` (stay local). The divide
/// rounds to nearest (`+ w_max/2`) so a blob that needs "just over" one
/// extra worker gets it. Integer-only — the EWMA is fixed-point nanos
/// (iamacoffeepot/aether#1128), no float on the flush path.
fn recruit_k(total_work: u64, max_group_work: u64, g: usize, w: usize) -> usize {
    if total_work <= wake_cost_nanos() || max_group_work == 0 {
        // Below the wake floor (or no measurable pole) — stay local.
        return 1;
    }
    // Round-to-nearest integer divide of Σw / w_max, integer-only.
    let raw = (total_work + max_group_work / 2) / max_group_work;
    let ceiling = g.min(w).max(1);
    // `raw >= 1` here (total_work > 0 and the round-half divide never
    // truncates a positive ratio to 0), but clamp the low end anyway so a
    // pathological zero can't escape.
    let k = usize::try_from(raw).unwrap_or(ceiling);
    k.clamp(1, ceiling)
}

/// Is a handler's EWMA mean trustworthy enough to size recruitment on,
/// given its mean-absolute-deviation? `true` when the spread is below the
/// [`MAD_CONFIDENCE_NUM`] / [`MAD_CONFIDENCE_DEN`] fraction of the mean —
/// a steady handler. `false` for a bimodal one (MAD as large as the mean),
/// whose mean poorly predicts the next dispatch. Integer ratio test, no
/// float. A zero mean (with a nonzero MAD) is treated as untrustworthy.
fn trustworthy_mad(mean_nanos: u64, mad_nanos: u64) -> bool {
    // mad / mean <= NUM / DEN  ⇔  mad * DEN <= mean * NUM (no division).
    mad_nanos.saturating_mul(MAD_CONFIDENCE_DEN) <= mean_nanos.saturating_mul(MAD_CONFIDENCE_NUM)
}

/// Resolve one mail's handler cost for the recruiter: its EWMA mean
/// (nanos) plus whether that estimate is *trustworthy*. A handler is
/// untrustworthy — and so the cost is `0` and `confident` is `false` —
/// when the cell is absent (`None`), a neutral seed (`samples == 0`,
/// known-but-never-run), or high-MAD (its spread is as large as its mean,
/// so the EWMA poorly predicts the next dispatch). One such cell in a fresh
/// group tips the whole flush to the width-gate fallback
/// (iamacoffeepot/aether#1178). A trustworthy cell returns
/// `(mean_nanos, true)`.
fn group_mail_cost(costs: &CostLookup<'_>, recipient: MailboxId, kind: KindId) -> (u64, bool) {
    match costs.get(recipient, kind) {
        Some(sample)
            if sample.samples > 0 && trustworthy_mad(sample.mean_nanos, sample.mad_nanos) =>
        {
            (sample.mean_nanos, true)
        }
        // Absent, neutral seed, or high-MAD: unknown cost.
        _ => (0, false),
    }
}

/// One recipient's mail within a blob, behind a closeable buffer. See the
/// module doc (§Closeable per-group buffer) for the SPSC drain-loop /
/// close-on-empty FIFO contract.
struct GroupBuf {
    /// Set by the claiming worker once it drains the buffer empty. A
    /// producer push after this is rejected (deposited via `route_mail`).
    closed: bool,
    /// Pending mail in send order. Drained in batches by the claiming
    /// worker; appended by the producer until `closed`.
    mails: Vec<Mail>,
}

struct Group {
    recipient: MailboxId,
    /// SPSC spinlock guarding [`Self::buf`]: `false` = free, `true` = held.
    /// Replaces a `std::sync::Mutex` — on macOS that lazily creates a
    /// `pthread_mutex_t` on first lock and destroys it on drop, and the
    /// #1140 profile found that per-group create + destroy churn (a fresh
    /// group per flush) to be the dominant blob-machinery cost. A bare
    /// atomic flag has no OS object: zero-cost to construct and drop.
    /// Contention is rare by construction (one producer, one cursor-winning
    /// worker — §Closeable per-group buffer), so the spin almost never loops.
    lock: AtomicBool,
    /// Accumulated handler cost for this group: `Σ cost(recipient, kᵢ)`
    /// over every mail pushed into it, summing iamacoffeepot/aether#1128's
    /// per-handler EWMA mean (nanos). A group coalesces mails of different
    /// kinds (the `index` dedups by recipient, not by kind — `append_flush`),
    /// so its work is a sum, not a single value — the recruiter's `w_max`
    /// per-group term (iamacoffeepot/aether#1178). Written by the single
    /// producer (on push, fresh or appended), never read by a draining
    /// worker; an `AtomicU64` so the producer accumulates it without taking
    /// the buffer spinlock and without `&mut Group` (the slot hands out
    /// `&Group`). `Relaxed` — single-writer, no ordering coupling to the
    /// buffer.
    work: AtomicU64,
    buf: UnsafeCell<GroupBuf>,
}

// SAFETY: `buf` is touched only while holding `lock` ([`Group::with_buf`]
// CAS-acquires before access, release-stores after), so the producer's
// `push` and the one cursor-winning worker's `take_or_close` never access it
// concurrently; `recipient` is immutable. So `&Group` is safe to share
// across the producer + worker threads.
unsafe impl Sync for Group {}

impl Group {
    fn new(recipient: MailboxId, mails: Vec<Mail>, work: u64) -> Self {
        Self {
            recipient,
            lock: AtomicBool::new(false),
            work: AtomicU64::new(work),
            buf: UnsafeCell::new(GroupBuf {
                closed: false,
                mails,
            }),
        }
    }

    /// Producer-only: accumulate `cost` (nanos) onto this group's running
    /// work total when a later mail appends to an already-published group.
    /// `saturating_add` so a corrupt cost can't wrap (the EWMA is bounded
    /// fixed-point nanos, ~9 orders below `u64::MAX`, so this only ever
    /// matters defensively — matching the `saturating_*` idiom elsewhere in
    /// this module). `Relaxed`: single writer (the producer), and no
    /// draining worker reads `work`.
    fn add_work(&self, cost: u64) {
        let prior = self.work.load(Ordering::Relaxed);
        self.work
            .store(prior.saturating_add(cost), Ordering::Relaxed);
    }

    /// The group's accumulated work `Σ cost(recipient, kᵢ)` (nanos).
    /// Producer-side read for the recruiter's `w_max` term.
    fn work(&self) -> u64 {
        self.work.load(Ordering::Relaxed)
    }

    /// Run `f` over the buffer under the spinlock. The closures below are
    /// allocation-light and panic-free (`Vec` ops abort, not unwind, on OOM),
    /// so the lock is always released — no poison state to model.
    fn with_buf<R>(&self, f: impl FnOnce(&mut GroupBuf) -> R) -> R {
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
        // SAFETY: the CAS above acquired the lock, so we hold exclusive
        // access to `buf` until the release-store below.
        let r = f(unsafe { &mut *self.buf.get() });
        self.lock.store(false, Ordering::Release);
        r
    }

    /// Producer: append `mail`, or hand it back (`Err`) if the group has
    /// been drained+closed — the caller deposits it through `route_mail`,
    /// where it lands strictly after everything the claiming worker
    /// dispatched (the close barrier).
    #[allow(
        clippy::result_large_err,
        reason = "the rejected Mail moves back to the caller for deposit on the cold closed-group path; boxing it would add a cold-path alloc and break the Mail-by-value convention"
    )]
    fn push(&self, mail: Mail) -> Result<(), Mail> {
        self.with_buf(|b| {
            if b.closed {
                Err(mail)
            } else {
                b.mails.push(mail);
                Ok(())
            }
        })
    }

    /// Claiming worker: take the next batch of pending mail (send order),
    /// or `None` once the buffer is empty — at which point this call closes
    /// the group (the FIFO barrier). The worker loops until `None`.
    fn take_or_close(&self) -> Option<Vec<Mail>> {
        self.with_buf(|b| {
            if b.mails.is_empty() {
                b.closed = true;
                None
            } else {
                Some(mem::take(&mut b.mails))
            }
        })
    }

    /// Consume a group whose [`Lifecycle::publish`] failed (retired / full),
    /// so it was never claimable, and return its mail for the producer to
    /// roll into a fresh blob. No worker can have touched it (the cursor
    /// never reached its index), so taking it by value is sound.
    fn into_mails(self) -> Vec<Mail> {
        self.buf.into_inner().mails
    }
}

/// A write-once slot in a blob's group array. Backed by a bare
/// `UnsafeCell<MaybeUninit<Group>>` rather than a `OnceLock`: the per-slot
/// `Once` synchronization a `OnceLock` performs is **redundant** here,
/// because publication is already ordered by the lifecycle word — the
/// producer writes the slot before [`Lifecycle::publish`]'s release of
/// `len`, and a worker reads it only after [`Lifecycle::claim`]'s acquire
/// observes `len > idx` (the `blob_lifecycle` publication-ordering note).
/// Dropping the `OnceLock` drops a CAS (write) + an acquire load (read)
/// per group.
struct GroupSlot {
    cell: UnsafeCell<MaybeUninit<Group>>,
}

// SAFETY: `BlobWork` is shared across worker threads via `Arc`, so its
// group slots must be `Sync`. Concurrent access is sound by the
// write-once / publish-before-claim discipline: (1) the single producer
// writes each slot exactly once, before the `publish` whose release-store
// of `len` makes the slot claimable; (2) a worker reads a slot only after
// its `claim` acquire-loaded `len > idx`, synchronizing-with that publish,
// so the write happens-before the read; (3) the slot itself is never
// re-written after that initial write (the `Group`'s mail buffer mutates
// behind its own `Mutex`, not the slot). No two accesses to one slot are
// unsynchronized.
unsafe impl Sync for GroupSlot {}

impl GroupSlot {
    const fn empty() -> Self {
        Self {
            cell: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Producer: write the group into this slot.
    ///
    /// # Safety
    /// Caller is the single producer, this slot has not been written, and
    /// the write is sequenced before the `publish` that makes the slot
    /// claimable (see the type's `Sync` contract).
    unsafe fn write(&self, group: Group) {
        // SAFETY: the `# Safety` contract above — single producer, unwritten
        // slot, sequenced before the publish that makes it claimable.
        unsafe { (*self.cell.get()).write(group) };
    }

    /// Read the group.
    ///
    /// # Safety
    /// The slot is initialized and either published (the caller's `claim`
    /// acquired `len > idx`) or being read by the producer that wrote it;
    /// the slot is never mutated after the write, so the shared reference
    /// is sound.
    unsafe fn get(&self) -> &Group {
        // SAFETY: the `# Safety` contract above — slot initialized and
        // published (or read by the producer that wrote it), never mutated.
        unsafe { (*self.cell.get()).assume_init_ref() }
    }

    /// Producer: move the group back out — `publish` failed, so the slot
    /// was never claimable. The slot is logically uninitialized afterward;
    /// the caller must not read or drop it again.
    ///
    /// # Safety
    /// The slot was written this flush and never published, so no worker
    /// can have claimed it; the producer reclaims sole ownership.
    unsafe fn take(&self) -> Group {
        // SAFETY: the `# Safety` contract above — slot written this flush,
        // never published, producer reclaims sole ownership.
        unsafe { (*self.cell.get()).assume_init_read() }
    }
}

/// Outcome of folding one flush into a blob ([`BlobWork::append_flush`]).
struct FlushOutcome {
    /// Mail that did not fit (blob retired or group array full) — the
    /// producer rolls it into a fresh blob. Always *new* recipients (seen
    /// recipients push to existing groups), so a rolled blob shares no
    /// recipient with this one.
    leftover: Vec<Mail>,
    /// Number of new groups this flush published — the width that drives
    /// the confidence-fallback (width) recruitment gate.
    fresh_groups: usize,
    /// `Σw`: the summed handler cost (nanos) of this flush's **fresh
    /// groups** — the cost-aware recruiter's numerator
    /// (iamacoffeepot/aether#1178). Scoped to fresh groups so it matches
    /// `fresh_groups` (the recruit decision is per-flush) and pairs
    /// consistently with `max_group_work` (same scope). Existing groups a
    /// later flush appends to keep their *own* running `Group::work` total
    /// but do not re-enter another flush's `total_work`.
    total_work: u64,
    /// `w_max`: the largest single fresh group's accumulated work (nanos)
    /// — the longest pole, bounding how spreadable this flush's work is.
    max_group_work: u64,
    /// `true` when every fresh group's contributing handler cells were
    /// **trustworthy** (seeded, run at least once, low MAD). `false` if any
    /// contributing cell was unknown cost (a neutral seed, an absent
    /// handler, or high-MAD) — the recruiter then ignores `total_work` /
    /// `max_group_work` and falls back to the width gate for this blob.
    cost_confident: bool,
}

/// One producer's shared cooperative blob. Constructed empty (sized to a
/// flush) and filled via [`Self::append_flush`]; drained by any number of
/// workers via the [`Drainable`] impl.
pub struct BlobWork {
    lifecycle: Lifecycle,
    /// Fixed-capacity group array. The initialized prefix is exactly
    /// `[0, lifecycle.len())`: the producer writes a slot before the
    /// `publish` that advances `len`, and a failed publish takes its staged
    /// groups back out (see [`Self::append_flush`]), so `len` always tracks
    /// the initialized prefix — which [`Drop`] relies on. Sized to the
    /// first flush (with a [`GROUP_CAP_MIN`] floor); the producer rolls a
    /// fresh blob on overflow.
    groups: Box<[GroupSlot]>,
    mailer: Arc<Mailer>,
    /// Where a recipient that yields mid-drain ([`CycleResult::Requeue`]) is
    /// re-scheduled — the same path a normal wake uses.
    sink: WakeSink,
}

impl BlobWork {
    /// An empty blob with a `cap`-slot group array (all slots uninit).
    fn empty(cap: usize, mailer: Arc<Mailer>, sink: WakeSink) -> Arc<Self> {
        let groups = (0..cap).map(|_| GroupSlot::empty()).collect();
        Arc::new(Self {
            lifecycle: Lifecycle::new(0),
            groups,
            mailer,
            sink,
        })
    }

    /// Producer (single-threaded for a given blob): fold one flush's mail
    /// in. `index` is this blob's producer-private recipient → group-index
    /// map (held by the [`BlobProducer`]). New recipients become new groups
    /// (written then published); seen recipients push onto their existing
    /// group's buffer (or, if it has been closed, deposit through
    /// `route_mail`). Returns the leftover (overflow / retired), the
    /// fresh-group count, and the cost-aware recruit signals (`Σw` /
    /// `w_max` / confidence) summed over this flush's fresh groups
    /// (iamacoffeepot/aether#1178).
    fn append_flush(
        &self,
        routed: Vec<Mail>,
        index: &mut FxHashMap<MailboxId, usize>,
    ) -> FlushOutcome {
        // Single pass: a recipient already in `index` (a prior flush, or one
        // staged earlier in *this* flush) pushes onto its existing group; a
        // brand-new recipient stages a fresh group at the next free index,
        // updating `index` so a repeat in the same flush coalesces onto it
        // (per-recipient FIFO, one group per recipient). `index` itself is
        // the in-flush dedup, so there is no separate bucket map / order
        // vector to allocate. `peek_len` is a plain load — `len` has a
        // single writer (this producer).
        let base = self.lifecycle.peek_len();
        let cap = self.groups.len();
        let mut staged = 0usize;
        let mut leftover: Vec<Mail> = Vec::new();
        // iamacoffeepot/aether#1178: one read-lock over the cost table for
        // the whole flush — every mail's `(recipient, kind)` cost resolves
        // through this guard, never a `read()` per mail (the rejected
        // per-mail point lookup). Confidence starts trustworthy and is
        // cleared by the first unknown-cost cell contributing to a *fresh*
        // group (this flush's claimable parallelizable work — the recruit
        // scope, step 5).
        let costs = self.mailer.cost_table().lookup();
        let mut cost_confident = true;
        for mail in routed {
            if let Some(&g) = index.get(&mail.recipient) {
                // Existing group — push in send order; a closed (drained)
                // group deposits through the router instead. SAFETY: `g` is
                // in `index`, so it was written this flush or a prior one by
                // this producer; the slot is initialized.
                let group = unsafe { self.groups[g].get() };
                // Accumulate cost onto the group's running total only when it
                // is *fresh this flush* (`g >= base`); an append onto a
                // prior-flush group keeps its own `Group::work` faithful for
                // diagnostics but does not re-enter this flush's `Σw` scope.
                if g >= base {
                    let (cost, confident) = group_mail_cost(&costs, mail.recipient, mail.kind);
                    group.add_work(cost);
                    cost_confident &= confident;
                }
                if let Err(mail) = group.push(mail) {
                    self.mailer.push(mail);
                }
            } else if base + staged >= cap {
                // Group array full — roll the rest into a fresh blob. Only
                // *new* recipients reach here (seen ones push above), so a
                // rolled blob shares no recipient with this one.
                leftover.push(mail);
            } else {
                let recipient = mail.recipient;
                let (cost, confident) = group_mail_cost(&costs, recipient, mail.kind);
                cost_confident &= confident;
                // SAFETY: producer-only; `base + staged` advances
                // contiguously and is unwritten, and the write is sequenced
                // before the `publish` below.
                unsafe {
                    self.groups[base + staged].write(Group::new(recipient, vec![mail], cost));
                }
                index.insert(recipient, base + staged);
                staged += 1;
            }
        }

        match self.lifecycle.publish(staged) {
            Published::Ok => {
                // Sum `Σw` / `w_max` over the freshly-published groups
                // `[base, base + staged)` — the recruit scope (step 5).
                let mut total_work = 0u64;
                let mut max_group_work = 0u64;
                for j in 0..staged {
                    // SAFETY: slot `base + j` was just published (`len`
                    // advanced past it), so it is initialized; `work()` is a
                    // producer-side relaxed load on the slot we wrote.
                    let group = unsafe { self.groups[base + j].get() };
                    let w = group.work();
                    total_work = total_work.saturating_add(w);
                    max_group_work = max_group_work.max(w);
                }
                FlushOutcome {
                    leftover,
                    fresh_groups: staged,
                    total_work,
                    max_group_work,
                    cost_confident,
                }
            }
            Published::Retired | Published::Full => {
                // The blob retired (or hit the wire ceiling) between staging
                // and publish: the staged groups never became claimable.
                // Take each back out (restoring the initialized prefix to
                // `[0, len)`) and roll its mail into a fresh blob.
                for j in 0..staged {
                    // SAFETY: slot `base + j` was written this flush and never
                    // published (`len` did not advance), so no worker claimed
                    // it; the producer reclaims sole ownership.
                    let group = unsafe { self.groups[base + j].take() };
                    index.remove(&group.recipient);
                    leftover.extend(group.into_mails());
                }
                // No fresh groups published — no recruit, no cost signal.
                FlushOutcome {
                    leftover,
                    fresh_groups: 0,
                    total_work: 0,
                    max_group_work: 0,
                    cost_confident: false,
                }
            }
        }
    }

    /// Dispatch one group's mail to its recipient, draining the closeable
    /// buffer in batches until empty (then the buffer self-closes — the
    /// FIFO barrier). Each batch's mail dispatches in send order via the
    /// #1135 per-mail fast path. `received` is the blob-pickup stamp this
    /// worker took at `run_cycle` entry (iamacoffeepot/aether#1150),
    /// threaded onto each dispatched mail's `t_enqueue`.
    fn dispatch_group(&self, group: &Group, budget: BatchBudget, received: Nanos) {
        while let Some(batch) = group.take_or_close() {
            for mail in batch {
                self.dispatch_one(group.recipient, mail, budget, received);
            }
        }
    }

    /// The iamacoffeepot/aether#1135 per-mail demux step: seize the
    /// recipient and dispatch in place, or deposit through `route_mail`.
    /// `received` is this worker's blob-pickup stamp
    /// (iamacoffeepot/aether#1150), carried onto the in-place seed's
    /// `t_enqueue`.
    fn dispatch_one(&self, recipient: MailboxId, mail: Mail, budget: BatchBudget, received: Nanos) {
        let lookup = self.mailer.registry().route_lookup(mail.kind, recipient);
        // Direct-dispatch only a ref-free kind whose recipient is a
        // `Pooled` slot we win the seize on; everything else deposits.
        let seized = if lookup.ref_schema.is_some() {
            None
        } else {
            lookup.seize.as_ref().and_then(SeizeHandle::try_seize)
        };
        match seized {
            Some(slot) => {
                let seed = Envelope {
                    kind: mail.kind,
                    kind_name: lookup.kind_name,
                    origin: None,
                    sender: mail.reply_to,
                    payload: mail.payload,
                    count: mail.count,
                    mail_id: mail.mail_id,
                    root: mail.root,
                    parent_mail: mail.parent_mail,
                    // iamacoffeepot/aether#1150: the blob-pickup stamp, not
                    // a fresh `now` — `now` here ≈ `t_received` and collapsed
                    // residence to ~0. With the pickup instant, the recipient's
                    // `Received` reads a real `t_received − t_enqueue` drain.
                    t_enqueue: received,
                    enqueue_depth: 0,
                };
                match slot.seize_and_run(seed, budget) {
                    CycleResult::Requeue => self.sink.schedule(slot),
                    CycleResult::Idle | CycleResult::Closed => {}
                }
            }
            None => self.mailer.push(mail),
        }
    }
}

impl Drainable for BlobWork {
    fn run_cycle(&self, budget: BatchBudget) -> CycleResult {
        // iamacoffeepot/aether#1150: the blob-received stamp. One clock
        // read marks when this worker picked up the blob and began
        // draining; every mail it dispatches this cycle inherits it as
        // `t_enqueue`, so `t_received − t_enqueue` measures where in the
        // drain that mail's handler entry landed (the in-blob
        // serialization a serial fan-out pays), not ~0. Stamped per
        // `run_cycle`, so a recruited sibling worker anchors its own
        // share of the groups against its own pickup instant.
        let received = self.mailer.now_nanos();
        // Drain to cursor exhaustion: a worker that picks up the blob runs
        // it in full, claiming and dispatching every group it wins off the
        // shared cursor until the cursor is drained. The parallelism is
        // cooperative — recruitment puts N copies of this blob in flight, so
        // N workers race the one cursor and split the groups between them —
        // not a per-worker yield. A blob is a finite, one-shot fan-out, so
        // (unlike an actor's ongoing inbox) it needs no fairness throttle;
        // the per-recipient `budget` still bounds each recipient's own inbox
        // drain inside `dispatch_group`. Late appends past the cursor are
        // picked up by the producer's re-submit on the next flush.
        while let Some(g) = self.lifecycle.claim() {
            // SAFETY: `claim` acquire-loaded `len > g`, synchronizing-with
            // the `publish` that wrote slot `g`; the slot is initialized and
            // never mutated after that write.
            let group = unsafe { self.groups[g].get() };
            self.dispatch_group(group, budget, received);
            self.lifecycle.complete();
        }
        CycleResult::Idle
    }

    fn label(&self) -> &'static str {
        "blob"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for BlobWork {
    fn drop(&mut self) {
        // The initialized slots are exactly `[0, len)` (see the `groups`
        // field doc): each is a `Group` that must be dropped — a bare
        // `MaybeUninit` would otherwise leak it. `[len, cap)` are uninit.
        // All `Arc` refs are gone (we are in `Drop`), so no slot is
        // concurrently accessed.
        let len = self.lifecycle.peek_len();
        for slot in &self.groups[..len] {
            // SAFETY: slot `< len` is initialized and no longer shared.
            unsafe { (*slot.cell.get()).assume_init_drop() };
        }
    }
}

/// One producing actor's blob lifecycle: keeps a single active blob,
/// appends each flush to it (rolling a fresh one when it retires or
/// overflows), and recruits drainers. Lives on the actor's [`NativeBinding`]
/// and is driven only from the actor's own thread, so `&mut self` access is
/// single-threaded.
pub struct BlobProducer {
    mailer: Arc<Mailer>,
    sink: WakeSink,
    /// The active blob + its producer-private recipient → group-index map.
    /// `None` until the first flush, and after a retired blob is dropped.
    active: Option<(Arc<BlobWork>, FxHashMap<MailboxId, usize>)>,
}

impl BlobProducer {
    /// Build a producer over the pool's [`WakeSink`] and the shared
    /// [`Mailer`].
    pub fn new(mailer: Arc<Mailer>, sink: WakeSink) -> Self {
        Self {
            mailer,
            sink,
            active: None,
        }
    }

    /// Fold one flush's routed mail into the active blob (or fresh blobs),
    /// scheduling a drainer and broadcast-recruiting siblings for wide
    /// fan-outs. Called on the producing actor's thread.
    pub fn flush(&mut self, routed: Vec<Mail>) {
        let mut pending = routed;
        while !pending.is_empty() {
            // Ensure a live (un-retired) active blob, sized to the pending
            // mail if we have to make a fresh one.
            let need_new = match &self.active {
                Some((blob, _)) => blob.lifecycle.is_retired(),
                None => true,
            };
            if need_new {
                let cap = group_cap_for(&pending);
                let blob = BlobWork::empty(cap, Arc::clone(&self.mailer), self.sink.clone());
                self.active = Some((blob, FxHashMap::default()));
            }

            let (blob, index) = self.active.as_mut().expect("active set above");
            let outcome = blob.append_flush(mem::take(&mut pending), index);
            let blob_arc: Arc<BlobWork> = Arc::clone(blob);
            let blob_dyn: Arc<dyn Drainable> = blob_arc;

            // Always schedule a drainer so newly-published groups (and any
            // pushes onto still-open groups) are picked up even if every
            // prior worker already drained past the cursor and dropped its
            // copy. The own-deque schedule keeps a cheap fan-out local
            // (the #1116 win); a heavy / wide fan-out additionally
            // broadcast-recruits siblings, sized by the cost-aware
            // recruiter (iamacoffeepot/aether#1178).
            self.sink.schedule(Arc::clone(&blob_dyn));
            let extra = recruit_extra(&outcome, self.sink.workers());
            if extra > 0 {
                self.sink.recruit(&blob_dyn, extra);
            }

            pending = outcome.leftover;
            // Non-empty leftover means the active blob could not take these
            // groups — its array is full (or it retired mid-publish). Detach
            // it so the remainder rolls into a fresh blob next iteration;
            // otherwise we'd re-append to the same full blob forever. The
            // detached blob stays alive via its in-flight `Arc` copies until
            // drained, and the leftover is always *new* recipients, so the
            // fresh blob shares none of its groups.
            if !pending.is_empty() {
                self.active = None;
            }
        }
    }
}

/// Size a fresh blob's group array. The mail count is an upper bound on the
/// distinct-recipient count (the real group count), so sizing to it —
/// clamped to the [`GROUP_CAP_MIN`] floor and the wire ceiling — never
/// under-sizes the first flush, and avoids a throwaway `HashSet` built just
/// to count distinct recipients on every flush. Over-sizing when a flush
/// has intra-flush duplicate recipients is harmless headroom.
fn group_cap_for(routed: &[Mail]) -> usize {
    routed.len().clamp(GROUP_CAP_MIN, MAX_GROUPS)
}

/// How many *sibling* workers to broadcast-recruit for a flush's outcome —
/// the value passed to [`WakeSink::recruit`] (`K - 1`, since the producer
/// drains its own [`WakeSink::schedule`] copy). `w` is the pool worker
/// count.
///
/// When the flush's fresh-group cost is **trustworthy**
/// (iamacoffeepot/aether#1178), size by [`recruit_k`] — the cost-aware
/// `Σw / w_max` form — and still bound the injector churn by `recruit_cap`
/// (over-recruiting past the worker count adds no parallelism). When the
/// cost is **unknown** (any contributing cell a neutral seed / absent /
/// high-MAD), fall back to the original **width gate**: recruit only when
/// the fresh-group count clears `recruit_min`, capped by `recruit_cap`.
/// Width is the confidence fallback, no longer the primary signal.
fn recruit_extra(outcome: &FlushOutcome, w: usize) -> usize {
    if outcome.fresh_groups == 0 {
        return 0;
    }
    if outcome.cost_confident {
        let k = recruit_k(
            outcome.total_work,
            outcome.max_group_work,
            outcome.fresh_groups,
            w,
        );
        k.min(recruit_cap()).saturating_sub(1)
    } else if outcome.fresh_groups >= recruit_min() {
        // Confidence fallback: the unchanged width gate.
        outcome.fresh_groups.min(recruit_cap()).saturating_sub(1)
    } else {
        0
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::collection_is_never_read,
    clippy::cast_possible_truncation,
    reason = "test setup: unwraps signal failure; fixture Vecs are held only to keep seize-handle Weaks alive; small loop indices cast to a marker byte"
)]
mod tests {
    use super::*;
    use crate::mail::Registry;
    use crate::mail::registry::{InboxHandler, OwnedDispatch};
    use crate::mail::{KindId, MailRef};
    use crate::scheduler::{SeizeSeed, SlotState, SpinPark};
    use crate::test_util::fresh_substrate;
    use aether_data::{KindDescriptor, SchemaCell, SchemaType};
    use crossbeam_deque::{Injector, Steal};
    use std::sync::mpsc;

    fn wake_sink(injector: &Arc<Injector<Arc<dyn Drainable>>>) -> WakeSink {
        WakeSink::new(Arc::clone(injector), Arc::new(SpinPark::new()), 8)
    }

    /// Drain the injector by running every queued `Drainable` to `Idle`
    /// (re-running on `Requeue`), simulating the worker pool from the test
    /// thread. Returns the number of `run_cycle` calls.
    fn drain_injector(injector: &Injector<Arc<dyn Drainable>>) -> usize {
        let mut runs = 0;
        loop {
            match injector.steal() {
                Steal::Success(slot) => {
                    runs += 1;
                    if slot.run_cycle(BatchBudget::standard()) == CycleResult::Requeue {
                        injector.push(slot);
                    }
                }
                Steal::Retry => {}
                Steal::Empty => return runs,
            }
        }
    }

    /// Register an inbox under `name` that forwards each delivered mail's
    /// first payload byte onto `tx`; returns the registered mailbox id.
    fn register_byte_forwarding_inbox(
        registry: &Registry,
        name: &str,
        tx: mpsc::Sender<u8>,
    ) -> MailboxId {
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: OwnedDispatch| {
            let _ = tx.send(d.payload.bytes()[0]);
        });
        registry.register_inbox(name, handler)
    }

    fn mail_to(recipient: MailboxId, byte: u8) -> Mail {
        Mail::new(recipient, KindId(7), MailRef::from(vec![byte]), 1)
    }

    /// A `Pooled`-shaped recipient fixture (mirrors the #1135 demux test
    /// fixture): a real [`SlotState`] so a [`SeizeHandle`] can drive the
    /// seize CAS, recording each direct-dispatched seed's first byte. Parks
    /// the slot back to `Idle` after each seed so the next mail can seize.
    struct SeizableSink {
        state: Arc<SlotState>,
        direct: mpsc::Sender<u8>,
    }
    impl Drainable for SeizableSink {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }
        fn seize_and_run(&self, seed: SeizeSeed, _budget: BatchBudget) -> CycleResult {
            let _ = self.direct.send(seed.payload.bytes()[0]);
            self.state.mark_idle();
            CycleResult::Idle
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Register a deposit inbox under `name` and install a seize handle over
    /// a [`SeizableSink`]. Returns the id + the fixture (keeps the slot
    /// `Weak` alive) + a receiver for each path.
    fn seizable_recipient(
        registry: &Registry,
        name: &str,
    ) -> (
        MailboxId,
        Arc<SeizableSink>,
        mpsc::Receiver<u8>,
        mpsc::Receiver<u8>,
    ) {
        let (deposit_tx, deposit_rx) = mpsc::channel::<u8>();
        let id = register_byte_forwarding_inbox(registry, name, deposit_tx);
        let (direct_tx, direct_rx) = mpsc::channel::<u8>();
        let fixture = Arc::new(SeizableSink {
            state: Arc::new(SlotState::new()),
            direct: direct_tx,
        });
        let slot_dyn: Arc<dyn Drainable> = fixture.clone();
        let installed = registry.install_seize_handle(
            id,
            SeizeHandle::new(Arc::clone(&fixture.state), Arc::downgrade(&slot_dyn)),
        );
        assert!(installed, "seize handle installs on a live Inbox entry");
        (id, fixture, direct_rx, deposit_rx)
    }

    /// A single-recipient fan-out: each leaf gets its mail exactly once, via
    /// the in-place seize path (closure deposit untouched).
    #[test]
    fn fanout_dispatches_each_recipient_once_in_place() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let mut fixtures = Vec::new();
        let mut directs = Vec::new();
        let mut routed = Vec::new();
        for i in 0..6u8 {
            let (id, fix, direct_rx, _dep) = seizable_recipient(&registry, &format!("r{i}"));
            routed.push(mail_to(id, i));
            fixtures.push(fix);
            directs.push(direct_rx);
        }

        let mut producer = BlobProducer::new(Arc::clone(&mailer), wake_sink(&injector));
        producer.flush(routed);
        drain_injector(&injector);

        for (i, rx) in directs.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let want = i as u8;
            assert_eq!(
                rx.try_recv().ok(),
                Some(want),
                "leaf {i} dispatched in place once"
            );
            assert!(rx.try_recv().is_err(), "leaf {i} dispatched exactly once");
        }
    }

    /// Two mails to the **same** recipient in one flush form one group and
    /// dispatch in send order (per-recipient FIFO).
    #[test]
    fn same_recipient_grouped_in_send_order() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let (id, _fix, direct_rx, _dep) = seizable_recipient(&registry, "r");

        let mut producer = BlobProducer::new(Arc::clone(&mailer), wake_sink(&injector));
        producer.flush(vec![mail_to(id, 0), mail_to(id, 1), mail_to(id, 2)]);
        drain_injector(&injector);

        assert_eq!(direct_rx.try_recv().ok(), Some(0));
        assert_eq!(direct_rx.try_recv().ok(), Some(1));
        assert_eq!(direct_rx.try_recv().ok(), Some(2));
        assert!(direct_rx.try_recv().is_err());
    }

    /// A busy recipient (slot already `Running`) is deposited, not
    /// dispatched in place.
    #[test]
    fn busy_recipient_deposited() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let (id, fixture, direct_rx, deposit_rx) = seizable_recipient(&registry, "r");
        assert!(
            fixture.state.seize(),
            "mark the recipient busy before the demux"
        );

        let mut producer = BlobProducer::new(Arc::clone(&mailer), wake_sink(&injector));
        producer.flush(vec![mail_to(id, 9)]);
        drain_injector(&injector);

        assert_eq!(
            deposit_rx.try_recv().ok(),
            Some(9),
            "busy recipient deposited"
        );
        assert!(direct_rx.try_recv().is_err(), "not dispatched in place");
    }

    /// A closure-backed inbox (no slot) has no seize handle, so its mail is
    /// deposited through the router.
    #[test]
    fn closure_inbox_deposited() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let (tx, rx) = mpsc::channel::<u8>();
        let id = register_byte_forwarding_inbox(&registry, "sink", tx);

        let mut producer = BlobProducer::new(Arc::clone(&mailer), wake_sink(&injector));
        producer.flush(vec![mail_to(id, 3)]);
        drain_injector(&injector);

        assert_eq!(
            rx.try_recv().ok(),
            Some(3),
            "closure inbox receives via deposit"
        );
    }

    /// An ADR-0045 ref-carrying kind is never dispatched in place even to a
    /// free `Pooled` recipient — it is deposited so `route_mail` walks it.
    #[test]
    fn ref_kind_deposited() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let (id, _fix, direct_rx, deposit_rx) = seizable_recipient(&registry, "r");
        let ref_kind = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.blob.ref_kind".to_owned(),
                schema: SchemaType::Ref(SchemaCell::owned(SchemaType::Bytes)),
            })
            .expect("fresh ref kind registers");

        let mut producer = BlobProducer::new(Arc::clone(&mailer), wake_sink(&injector));
        producer.flush(vec![Mail::new(
            id,
            ref_kind,
            MailRef::from(vec![0u8, 0u8]),
            1,
        )]);
        drain_injector(&injector);

        assert!(
            direct_rx.try_recv().is_err(),
            "ref kind never dispatched in place"
        );
        assert_eq!(
            deposit_rx.try_recv().ok(),
            Some(0),
            "ref kind deposited for the ref-walk"
        );
    }

    /// Two recipients across two flushes: the second flush to a fresh
    /// recipient appends a new group; both deliver exactly once.
    #[test]
    fn second_flush_new_recipient_appends_group() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let (a, _fa, a_rx, _ad) = seizable_recipient(&registry, "a");
        let (b, _fb, b_rx, _bd) = seizable_recipient(&registry, "b");

        let mut producer = BlobProducer::new(Arc::clone(&mailer), wake_sink(&injector));
        producer.flush(vec![mail_to(a, 1)]);
        drain_injector(&injector);
        producer.flush(vec![mail_to(b, 2)]);
        drain_injector(&injector);

        assert_eq!(a_rx.try_recv().ok(), Some(1));
        assert_eq!(b_rx.try_recv().ok(), Some(2));
    }

    /// Overflowing the group array rolls the remainder into a fresh blob;
    /// every recipient still gets its mail exactly once. Flush 1 of
    /// `GROUP_CAP_MIN` distinct recipients sizes the array to exactly that
    /// (full); flush 2's brand-new recipients overflow and roll into a
    /// second blob. (Two flushes with no drain between also exercises the
    /// full-but-not-retired roll path — the case that previously looped.)
    #[test]
    fn overflow_rolls_fresh_blob_no_loss() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let mut producer = BlobProducer::new(Arc::clone(&mailer), wake_sink(&injector));
        // Keep every fixture (and its receivers) alive to drain time — the
        // seize handle holds only a `Weak` to the fixture, so a dropped
        // fixture would make `try_seize` upgrade to `None` and silently
        // deposit-then-drop.
        let mut fixtures = Vec::new();
        let mut rxs: Vec<(mpsc::Receiver<u8>, mpsc::Receiver<u8>)> = Vec::new();

        let mut routed1 = Vec::new();
        for i in 0..GROUP_CAP_MIN as u8 {
            let (id, fix, direct_rx, deposit_rx) = seizable_recipient(&registry, &format!("p{i}"));
            routed1.push(mail_to(id, i));
            fixtures.push(fix);
            rxs.push((direct_rx, deposit_rx));
        }
        producer.flush(routed1);
        let mut routed2 = Vec::new();
        for i in 0..5u8 {
            let (id, fix, direct_rx, deposit_rx) = seizable_recipient(&registry, &format!("q{i}"));
            routed2.push(mail_to(id, 100 + i));
            fixtures.push(fix);
            rxs.push((direct_rx, deposit_rx));
        }
        producer.flush(routed2);
        drain_injector(&injector);

        // Collect from both paths (all should be direct here, but be robust).
        let mut got: Vec<u8> = rxs
            .iter()
            .filter_map(|(d, p)| d.try_recv().ok().or_else(|| p.try_recv().ok()))
            .collect();
        got.sort_unstable();
        let mut want: Vec<u8> = (0..GROUP_CAP_MIN as u8).collect();
        want.extend((0..5u8).map(|i| 100 + i));
        want.sort_unstable();
        assert_eq!(
            got, want,
            "every recipient delivered exactly once across the roll"
        );
    }

    /// Empty flush is a no-op.
    #[test]
    fn empty_flush_noop() {
        let (_registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let mut producer = BlobProducer::new(Arc::clone(&mailer), wake_sink(&injector));
        producer.flush(Vec::new());
        assert_eq!(
            drain_injector(&injector),
            0,
            "no work scheduled for an empty flush"
        );
    }

    /// End-to-end under a live multi-worker pool: a wide fan-out recruits
    /// siblings, and every recipient is dispatched **exactly once** while
    /// many workers race the shared cursor. The exactly-once gate is the
    /// cursor CAS (each group to one worker) + the per-recipient seize.
    #[test]
    fn concurrent_pool_drain_delivers_each_recipient_once() {
        use crate::runtime::lifecycle::PanicAborter;
        use crate::scheduler::{Pool, PoolConfig};
        use std::thread;
        use std::time::{Duration, Instant};

        // A wide fan-out (> recruit_min) so the flush broadcast-recruits.
        // N < 256 keeps the per-recipient marker byte unique.
        const N: usize = 60;

        let pool = Pool::start(
            PoolConfig {
                workers: 8,
                ..PoolConfig::default()
            },
            Arc::new(PanicAborter),
        );
        let (registry, mailer) = fresh_substrate();
        let mut producer = BlobProducer::new(Arc::clone(&mailer), pool.wake_sink());

        let mut fixtures = Vec::new();
        let mut rxs = Vec::new();
        let mut routed = Vec::new();
        for i in 0..N {
            let (id, fix, direct_rx, deposit_rx) = seizable_recipient(&registry, &format!("c{i}"));
            #[allow(clippy::cast_possible_truncation)]
            let byte = i as u8;
            routed.push(Mail::new(id, KindId(7), MailRef::from(vec![byte]), 1));
            fixtures.push((fix, deposit_rx));
            rxs.push(direct_rx);
        }
        producer.flush(routed);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got: Vec<u8> = Vec::new();
        while got.len() < N && Instant::now() < deadline {
            for rx in &rxs {
                while let Ok(b) = rx.try_recv() {
                    got.push(b);
                }
            }
            if got.len() < N {
                thread::yield_now();
            }
        }

        let results = pool.shutdown_with_results();
        assert!(
            results.iter().all(thread::Result::is_ok),
            "no worker thread panicked during concurrent drain"
        );
        got.sort_unstable();
        let want: Vec<u8> = (0..N as u8).collect();
        assert_eq!(
            got, want,
            "each recipient dispatched exactly once under concurrent drain"
        );
    }

    /// Flake-soak duplicate (iamacoffeepot/aether#1137): the concurrent
    /// drain is timing-sensitive (cursor race + seize CAS across 8
    /// workers), so it gets a `flaky_` wrapper for `scripts/flake-soak.sh`.
    /// The original runs once in normal CI.
    #[test]
    fn flaky_concurrent_pool_drain_delivers_each_recipient_once() {
        concurrent_pool_drain_delivers_each_recipient_once();
    }

    // iamacoffeepot/aether#1178: the cost-aware recruiter. `recruit_k` is a
    // pure function over `(total_work, max_group_work, G, W)`; the canonical
    // cases below use work values well clear of the default
    // `C_WAKE_NANOS = 4300` so they don't depend on an env override.

    /// Trivial-wide: many tiny groups whose total work is below the wake
    /// break-even → `K = 1` (stay local, no recruit). This is the cost-aware
    /// form of the prior narrow-local win — a *cheap* fan-out stays serial
    /// regardless of width.
    #[test]
    fn recruit_k_trivial_wide_stays_local() {
        // 12 groups × 100ns = 1200ns total, under the 4300ns floor.
        assert_eq!(recruit_k(1_200, 100, 12, 8), 1);
        // Even a single tiny group is local.
        assert_eq!(recruit_k(100, 100, 1, 8), 1);
    }

    /// Heavy-narrow: a few heavy groups → `K = G` (recruits the full group
    /// count). This closes the documented width-gate gap — a heavy `<= 8`
    /// fan-out that the old gate left serial now parallelises.
    #[test]
    fn recruit_k_heavy_narrow_recruits_full_width() {
        // 3 groups × 50_000ns each: total 150_000, w_max 50_000 → round(3) = 3,
        // clamped to min(G=3, W=8) = 3.
        assert_eq!(recruit_k(150_000, 50_000, 3, 8), 3);
        // Two heavy groups, plenty of workers → 2.
        assert_eq!(recruit_k(100_000, 50_000, 2, 8), 2);
    }

    /// Balanced-wide: many equal groups → `K = min(G, W)`. Σw / w_max ≈ G,
    /// clamped by the worker count.
    #[test]
    fn recruit_k_balanced_wide_recruits_to_worker_cap() {
        // 20 equal groups of 10_000ns: round(20) = 20, clamped to W = 8.
        assert_eq!(recruit_k(200_000, 10_000, 20, 8), 8);
        // Same balance, fewer groups than workers → clamped to G.
        assert_eq!(recruit_k(60_000, 10_000, 6, 8), 6);
    }

    /// Skewed-wide: one fat pole plus many small groups → `K ≈ total /
    /// longest_pole`, *not* G. The longest pole bounds the achievable
    /// parallelism: extra workers past `total / w_max` can't beat the fat
    /// group's serial drain.
    #[test]
    fn recruit_k_skewed_wide_sizes_by_longest_pole() {
        // One 100_000ns pole + 10 × 1_000ns = 110_000 total, w_max 100_000 →
        // round(1.1) = 1, clamped to [1, min(11, 8)] = 1. The fat pole
        // dominates: no parallelism helps.
        assert_eq!(recruit_k(110_000, 100_000, 11, 8), 1);
        // A milder skew: 100_000 total over a 30_000ns pole → round(3.33) = 3.
        assert_eq!(recruit_k(100_000, 30_000, 11, 8), 3);
    }

    /// The wake gate is exact at the boundary: `total_work <= C_WAKE_NANOS`
    /// is local, one nano over recruits.
    #[test]
    fn recruit_k_wake_gate_boundary() {
        let c = wake_cost_nanos();
        assert_eq!(recruit_k(c, 10, 5, 8), 1, "exactly at the floor: local");
        // One nano over the floor with a small pole → recruits.
        assert!(
            recruit_k(c + 1, 10, 5, 8) > 1,
            "just over the floor: recruit"
        );
    }

    /// `recruit_k` never exceeds `min(G, W)` and never drops below 1.
    #[test]
    fn recruit_k_clamps_to_min_g_w() {
        // Huge Σw, tiny pole, but only 2 groups → clamped to G = 2.
        assert_eq!(recruit_k(1_000_000, 1, 2, 8), 2);
        // Same, but only 1 worker → clamped to W = 1.
        assert_eq!(recruit_k(1_000_000, 1, 50, 1), 1);
    }

    /// `trustworthy_mad`: a steady handler (MAD below the mean) is
    /// trustworthy; a bimodal one (MAD ≈ mean) is not.
    #[test]
    fn trustworthy_mad_gates_on_spread() {
        assert!(trustworthy_mad(10_000, 0), "zero MAD is steady");
        assert!(
            trustworthy_mad(10_000, 5_000),
            "half-mean MAD is trustworthy"
        );
        assert!(
            trustworthy_mad(10_000, 10_000),
            "MAD == mean is the boundary"
        );
        assert!(
            !trustworthy_mad(10_000, 10_001),
            "MAD over the mean is bimodal / untrustworthy"
        );
        assert!(!trustworthy_mad(0, 1), "a nonzero spread over a zero mean");
    }

    /// Seed a recipient's `KindId(7)` cost cell to a steady `mean_nanos`
    /// (folding the same sample repeatedly drives MAD to 0, so the cell is
    /// trustworthy). Returns nothing — the table is shared via the `Mailer`.
    fn seed_steady_cost(mailer: &Mailer, recipient: MailboxId, mean_nanos: u64) {
        let cells = mailer.cost_table().seed(recipient, &[KindId(7)]);
        for (_, cell) in &cells {
            for _ in 0..8 {
                cell.fold(mean_nanos);
            }
        }
    }

    /// `FlushOutcome` carries the hand-summed `Σw` / `w_max` over the fresh
    /// groups — including two mails to one recipient coalescing into one
    /// group whose work is the *sum* of both kinds' costs (here the same
    /// kind twice, so 2× the per-handler cost).
    #[test]
    fn flush_outcome_sums_group_work() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let (a, _fa, _a_dir, _a_dep) = seizable_recipient(&registry, "a");
        let (b, _fb, _b_dir, _b_dep) = seizable_recipient(&registry, "b");
        // a: 10_000ns/handler, two mails → group work 20_000.
        // b: 5_000ns/handler, one mail → group work 5_000.
        seed_steady_cost(&mailer, a, 10_000);
        seed_steady_cost(&mailer, b, 5_000);

        let blob = BlobWork::empty(8, Arc::clone(&mailer), wake_sink(&injector));
        let mut index = FxHashMap::default();
        let outcome = blob.append_flush(
            vec![mail_to(a, 0), mail_to(a, 1), mail_to(b, 2)],
            &mut index,
        );

        assert_eq!(outcome.fresh_groups, 2, "two distinct recipients");
        assert_eq!(outcome.total_work, 25_000, "Σw = a(2×10_000) + b(1×5_000)");
        assert_eq!(
            outcome.max_group_work, 20_000,
            "w_max is a's coalesced group (two mails summed)"
        );
        assert!(outcome.cost_confident, "both handlers seeded + steady");
    }

    /// A blob whose recipient has no EWMA history (a neutral-seed cell, or
    /// no cell at all) is "unknown cost": `cost_confident` is false, so the
    /// recruiter falls back to the width gate. Here a wide fan-out of unseen
    /// recipients clears `recruit_min` and recruits via the *fallback* path,
    /// not `recruit_k` (which would see Σw = 0 and stay local).
    #[test]
    fn unknown_cost_falls_back_to_width_gate() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        // recruit_min default is 9; build a 12-wide fan-out of unseeded
        // recipients so the width fallback fires while Σw stays 0.
        let mut routed = Vec::new();
        let mut fixtures = Vec::new();
        for i in 0..12u8 {
            let (id, fix, _dir, _dep) = seizable_recipient(&registry, &format!("u{i}"));
            routed.push(mail_to(id, i));
            fixtures.push(fix);
        }

        let blob = BlobWork::empty(16, Arc::clone(&mailer), wake_sink(&injector));
        let mut index = FxHashMap::default();
        let outcome = blob.append_flush(routed, &mut index);

        assert_eq!(outcome.fresh_groups, 12);
        assert_eq!(
            outcome.total_work, 0,
            "no seeded handlers → no measured work"
        );
        assert!(
            !outcome.cost_confident,
            "unseeded handlers mark the blob unknown-cost"
        );
        // The width fallback recruits (12 >= recruit_min 9): G.min(cap) - 1 =
        // 11 (the final W cap lives in WakeSink::recruit, as it did pre-#1178).
        let extra = recruit_extra(&outcome, 8);
        assert_eq!(extra, 11, "width fallback recruits G.min(recruit_cap) - 1");
    }

    /// A *narrow* unseeded fan-out (below `recruit_min`) takes the width
    /// fallback and recruits nobody — the cost-unknown narrow blob behaves
    /// exactly as the pre-#1178 width gate did.
    #[test]
    fn unknown_cost_narrow_stays_local_via_width() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let mut routed = Vec::new();
        let mut fixtures = Vec::new();
        for i in 0..4u8 {
            let (id, fix, _dir, _dep) = seizable_recipient(&registry, &format!("n{i}"));
            routed.push(mail_to(id, i));
            fixtures.push(fix);
        }

        let blob = BlobWork::empty(8, Arc::clone(&mailer), wake_sink(&injector));
        let mut index = FxHashMap::default();
        let outcome = blob.append_flush(routed, &mut index);

        assert!(!outcome.cost_confident);
        assert_eq!(
            recruit_extra(&outcome, 8),
            0,
            "narrow unknown-cost fan-out stays local (width fallback, < recruit_min)"
        );
    }

    /// A heavy *narrow* fan-out (below `recruit_min`) with trustworthy cost
    /// recruits its full group count — the headline #1178 win: the old width
    /// gate left this serial, the cost gate parallelises it without anyone
    /// touching `AETHER_BLOB_RECRUIT_MIN`.
    #[test]
    fn heavy_narrow_recruits_without_width_threshold() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let mut routed = Vec::new();
        let mut fixtures = Vec::new();
        // 3 recipients (well under recruit_min 9), each a heavy 50_000ns
        // handler — the documented heavy-narrow case.
        for i in 0..3u8 {
            let (id, fix, _dir, _dep) = seizable_recipient(&registry, &format!("h{i}"));
            seed_steady_cost(&mailer, id, 50_000);
            routed.push(mail_to(id, i));
            fixtures.push(fix);
        }

        let blob = BlobWork::empty(8, Arc::clone(&mailer), wake_sink(&injector));
        let mut index = FxHashMap::default();
        let outcome = blob.append_flush(routed, &mut index);

        assert!(outcome.cost_confident, "all three handlers seeded + steady");
        assert_eq!(outcome.fresh_groups, 3);
        // Σw = 150_000, w_max = 50_000 → K = 3, extra = 2 — recruited despite
        // being far below the width threshold.
        assert_eq!(
            recruit_extra(&outcome, 8),
            2,
            "heavy narrow fan-out recruits K-1 = 2 without the width gate"
        );
    }

    /// A *cheap* narrow fan-out with trustworthy cost stays `K = 1` (local) —
    /// the cost gate subsumes the width threshold for the right reason
    /// (cheapness), preserving the #1116 narrow-local win.
    #[test]
    fn cheap_narrow_stays_local_via_cost() {
        let (registry, mailer) = fresh_substrate();
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let mut routed = Vec::new();
        let mut fixtures = Vec::new();
        for i in 0..3u8 {
            let (id, fix, _dir, _dep) = seizable_recipient(&registry, &format!("c{i}"));
            // 200ns handlers → Σw = 600ns, under the 4300ns wake floor.
            seed_steady_cost(&mailer, id, 200);
            routed.push(mail_to(id, i));
            fixtures.push(fix);
        }

        let blob = BlobWork::empty(8, Arc::clone(&mailer), wake_sink(&injector));
        let mut index = FxHashMap::default();
        let outcome = blob.append_flush(routed, &mut index);

        assert!(outcome.cost_confident);
        assert_eq!(
            recruit_extra(&outcome, 8),
            0,
            "cheap fan-out stays local (Σw below the wake floor)"
        );
    }
}
