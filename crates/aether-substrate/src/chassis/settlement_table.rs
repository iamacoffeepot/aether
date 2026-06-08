//! Lock-free open-addressing settlement table (spike for
//! iamacoffeepot/aether#1059 — drop the per-hop settlement stripe mutex).
//!
//! [`super::settlement_counter::SettlementCounter`] guards its
//! `root -> CounterCell` map with a striped `Mutex`; the producer bench
//! (`settlement_counter::tests::bench_producer_hot_path`) measures
//! ~67 ns per `Sent`+`Finished` pair under that lock versus ~4 ns on the
//! bare [`CounterCell`] atomic. The lock guards only the *map structure*
//! (insert-on-first-event, drop-on-settle); the per-root count is already
//! a lock-free atomic word. This table removes the lock by making the map
//! itself lock-free, so every `record_*` call is a probe plus a bare
//! atomic — the ~4 ns floor on every access, not just a cached one.
//!
//! **Why this is tractable here when general lock-free open addressing is
//! not.** Two workload invariants (ADR-0080 / ADR-0086) collapse the two
//! hardest races:
//!
//! 1. *Unique, single-minter keys.* A root `MailId` is
//!    `(sender, correlation_id)` where `correlation_id` is a per-producer
//!    monotonic counter (one owner, `fetch_add`, never reset — see
//!    `actor::native::binding`). So two threads can never race to insert
//!    the *same* key. That kills the "find a tombstone to reuse while
//!    another thread inserts the same key further down the chain"
//!    duplicate-key hazard — insert is just "CAS-claim the first reusable
//!    slot; re-probe on loss", with no go-back-and-recheck-for-duplicate.
//!
//! 2. *Alive-during-mutation.* Any thread doing `add_*`/`sub_*` on root R
//!    holds at least one in-flight mail (or a settlement hold) under R,
//!    so R's count is at least 1 the whole time — its slot is stably
//!    `OCCUPIED` and
//!    cannot be the `(0,0)` transition that tombstones it. The
//!    tombstoning decrement is the *last* one, by the owner of the last
//!    in-flight unit, when no other thread has anything in-flight under R.
//!    So delete never races a live inc/dec; a finder's *own* key is never
//!    torn, and the seqlock re-validation below only ever rejects *other*
//!    slots probed past mid-reclaim.
//!
//! The exactly-once zero-transition is unchanged — it still lives on the
//! [`CounterCell`] atomic word, proven by the `settlement_counter` stress
//! tests. This table only decides *which* cell a root maps to; it never
//! touches the firing logic.
//!
//! **Contract (weaker than the striped counter — read this).** This table
//! relies on invariant 2 above and therefore does **not** tolerate a
//! *concurrent re-open*: a `record_sent(R)` racing the `record_finished`
//! that settles R. The settling decrement and the `OCCUPIED -> TOMBSTONE`
//! transition are two separate steps (no lock spans them), so a
//! re-increment landing between them would be clobbered by the tombstone
//! — silent settlement corruption. The striped `SettlementCounter`
//! survives this because it does the decrement and the map reclaim under
//! one lock. We give that up for lock-freedom, and it is sound *only
//! because the real dispatch workload never concurrently re-opens a
//! settled root*: you reach `record_sent(R)` either by minting R (once,
//! on a per-producer monotonic counter — no concurrency, R is brand new)
//! or while already handling a mail in-flight under R (so R's count is
//! at least 1 and cannot be settling). A root at `(0,0)` has no in-flight
//! mail,
//! so no handler can be running under it to emit a further send; the only
//! same-key recurrence is an actor reload minting the id afresh, which is
//! temporally separated from the old chain's settle and goes through the
//! clean `TOMBSTONE -> CLAIMING -> OCCUPIED` claim path. Promotion past
//! this spike must either (a) confirm this invariant holds on every
//! settlement path, or (b) add re-open robustness (a linked
//! decrement-and-tombstone), which is the genuinely hard lock-free piece.
//!
//! **Enforcement (not just documented).** The contract is guarded in code,
//! release-active and fail-fast (ADR-0063): claiming a slot asserts its
//! cell is `(0,0)` (`publish`), so a violation — a settling decrement that
//! tombstoned a live root — panics deterministically at the next reuse
//! rather than drifting settlement silently. A complementary debug-only
//! re-check at `tombstone` surfaces the same violation closer to its
//! source under test.
//!
//! **Slot lifecycle.** `EMPTY -> CLAIMING -> OCCUPIED -> TOMBSTONE ->
//! CLAIMING -> OCCUPIED -> ...`. The load-bearing rule: a slot **never
//! returns to `EMPTY`** once used. A finder treats `TOMBSTONE` as "keep
//! probing", so probe chains stay intact across reclaim, and a settled
//! slot is recycled in place by the next insert that probes to it — the
//! table doesn't fill from churn, only from *peak concurrent live roots*
//! (self-bounding per ADR-0086). Resize / overflow-to-a-cold-slot is a
//! follow-up; this spike sizes the table generously and treats a full
//! probe sweep as a fail-fast (it cannot happen at the occupancy the
//! stress tests or realistic fleets reach).

use std::sync::atomic::{AtomicU64, Ordering};

use aether_data::MailId;

use super::settlement_counter::CounterCell;

/// Slot state, stored in the low two bits of the `sv` word.
const STATE_MASK: u64 = 0b11;
const STATE_EMPTY: u64 = 0;
const STATE_CLAIMING: u64 = 1;
const STATE_OCCUPIED: u64 = 2;
const STATE_TOMBSTONE: u64 = 3;

/// One version unit in the `sv` word (the version occupies the high 62
/// bits). Every state transition bumps the version, so the seqlock read
/// (`sv1 == sv2` around the key load) detects a slot whose occupant
/// changed mid-read — and the 62-bit width makes wraparound within a
/// single reader's two loads impossible in practice.
const VERSION_UNIT: u64 = 1 << 2;

/// Default slot count (power of two). ~16K slots at 32 bytes each is
/// ~512 KB — orders of magnitude above the peak concurrent live-root
/// count a single substrate reaches (dozens–hundreds), so probe chains
/// stay near length 1 and the table never fills.
const DEFAULT_SLOTS: usize = 1 << 14;

/// A single open-addressing slot: the version-tagged state word, the
/// 128-bit root key split across two atomics, and the inline count cell.
///
/// The key halves are `AtomicU64` (not `UnsafeCell`) so reads need no
/// `unsafe` — a `Relaxed` load pair guarded by the `sv` seqlock is sound,
/// and on the target ISAs a `Relaxed` load is as cheap as a plain one.
#[derive(Debug)]
struct Slot {
    /// `(version << 2) | state`. A fresh `AtomicU64::new(0)` is
    /// `(version 0, STATE_EMPTY)` — the correct initial state.
    sv: AtomicU64,
    /// Key hi: the root's `sender` [`aether_data::MailboxId`] word.
    sender: AtomicU64,
    /// Key lo: the root's `correlation_id`.
    correlation: AtomicU64,
    /// The lock-free per-root count (settlement authority, unchanged).
    cell: CounterCell,
}

impl Slot {
    fn empty() -> Self {
        Self {
            sv: AtomicU64::new(0),
            sender: AtomicU64::new(0),
            correlation: AtomicU64::new(0),
            cell: CounterCell::zero(),
        }
    }
}

/// Bump the version of `sv` by one unit and set its state to
/// `new_state`. The version bump is what the seqlock read keys on.
#[inline]
fn with_state(sv: u64, new_state: u64) -> u64 {
    (sv & !STATE_MASK).wrapping_add(VERSION_UNIT) | new_state
}

/// Lock-free open-addressing `MailId -> CounterCell` table. Drop-in for
/// [`super::settlement_counter::SettlementCounter`]: same `record_*` /
/// `live_roots` / `held_open` surface, no lock on any path.
#[derive(Debug)]
pub struct SettlementTable {
    slots: Box<[Slot]>,
    mask: u64,
}

impl SettlementTable {
    /// Allocate a table with the default slot count (`DEFAULT_SLOTS`).
    #[must_use]
    pub fn new() -> Self {
        Self::with_slots(DEFAULT_SLOTS)
    }

    /// Allocate a table with `slots` slots, rounded up to a power of two
    /// (so the index mask is `slots - 1`). Minimum 2.
    #[must_use]
    pub fn with_slots(slots: usize) -> Self {
        let n = slots.next_power_of_two().max(2);
        let slots = (0..n).map(|_| Slot::empty()).collect::<Vec<_>>();
        Self {
            slots: slots.into_boxed_slice(),
            #[allow(clippy::cast_possible_truncation)]
            mask: n as u64 - 1,
        }
    }

    /// First probe index for `root`. Same mix as the striped counter's
    /// `stripe` so collision behaviour matches the incumbent.
    #[inline]
    #[allow(clippy::cast_possible_truncation)] // masked to < slot count
    fn home(&self, root: MailId) -> usize {
        let h = root.sender.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ root.correlation_id;
        (h & self.mask) as usize
    }

    /// Next probe index, wrapping at the table size.
    #[inline]
    #[allow(clippy::cast_possible_truncation)] // mask < slot count, fits usize
    fn next_index(&self, idx: usize) -> usize {
        (idx + 1) & self.mask as usize
    }

    /// Read `slot`'s key under the seqlock. Returns `Some((sender, corr))`
    /// only if the slot is `OCCUPIED` and its version is stable across the
    /// two `sv` loads (so the key is a consistent snapshot of one
    /// occupant). Returns `None` for any non-`OCCUPIED` state or a torn
    /// read (occupant changed mid-read — only possible for a slot the
    /// caller does *not* own, per the alive-during-mutation invariant).
    #[inline]
    fn read_key(slot: &Slot) -> Option<(u64, u64)> {
        let sv1 = slot.sv.load(Ordering::Acquire);
        if sv1 & STATE_MASK != STATE_OCCUPIED {
            return None;
        }
        let sender = slot.sender.load(Ordering::Relaxed);
        let correlation = slot.correlation.load(Ordering::Relaxed);
        let sv2 = slot.sv.load(Ordering::Acquire);
        (sv1 == sv2).then_some((sender, correlation))
    }

    /// Publish `root` into `slot`, which the caller has just CAS-won into
    /// the `CLAIMING` state (`claiming_sv` is the value it stored). Resets
    /// the count and releases the key + `OCCUPIED` state. Sound because
    /// `CLAIMING` is exclusive — no other thread reads the key or mutates
    /// the cell until the `Release` store makes `OCCUPIED` visible.
    #[inline]
    fn publish(slot: &Slot, claiming_sv: u64, root: MailId) {
        // Invariant guard (release-active, fail-fast per ADR-0063). A slot
        // becomes claimable only as EMPTY (cell never touched) or TOMBSTONE
        // (the settling decrement that tombstoned it observed `(0,0)`). A
        // non-zero cell here therefore means a *prior* settling decrement
        // tombstoned a still-live root — i.e. a `record_sent` re-opened a
        // root while it was settling, the one thing the module Contract
        // forbids. The slot is `CLAIMING` (exclusive) so this read races
        // nothing; the corruption persists until reuse, so this catches it
        // deterministically rather than letting settlement drift silently.
        assert_eq!(
            slot.cell.load(),
            (0, 0),
            "settlement table: claiming a slot whose cell is non-zero — a settling \
             decrement tombstoned a live root (concurrent re-open; see module Contract)"
        );
        slot.sender.store(root.sender.0, Ordering::Relaxed);
        slot.correlation
            .store(root.correlation_id, Ordering::Relaxed);
        slot.cell.reset();
        slot.sv.store(
            (claiming_sv & !STATE_MASK) | STATE_OCCUPIED,
            Ordering::Release,
        );
    }

    /// Find the cell for `root`, inserting it if absent. Never returns a
    /// borrow tied to anything but `&self` — slots live for the table's
    /// lifetime and never move, so the reference stays valid through any
    /// concurrent reclaim of *other* slots.
    ///
    /// # Panics
    /// Panics if a full probe sweep finds neither the key nor a reusable
    /// slot — the table is saturated. This spike sizes the table so that
    /// cannot happen; production sizing + cold-overflow is a follow-up.
    fn cell_for(&self, root: MailId) -> &CounterCell {
        let key = (root.sender.0, root.correlation_id);
        let home = self.home(root);
        'attempt: loop {
            let mut first_reusable: Option<usize> = None;
            let mut idx = home;
            for _ in 0..=self.mask {
                let slot = &self.slots[idx];
                match slot.sv.load(Ordering::Acquire) & STATE_MASK {
                    STATE_OCCUPIED if Self::read_key(slot) == Some(key) => return &slot.cell,
                    STATE_EMPTY => {
                        // Key is absent in [home..=idx] (slots never revert
                        // to EMPTY, so a present key would have been found
                        // before this EMPTY). Claim the first reusable slot
                        // seen, or this EMPTY. Unique keys mean no
                        // concurrent same-key insert to recheck for.
                        let target = first_reusable.unwrap_or(idx);
                        if self.try_claim(target, root) {
                            return &self.slots[target].cell;
                        }
                        // Lost the slot to another key's claim → re-probe.
                        continue 'attempt;
                    }
                    STATE_TOMBSTONE if first_reusable.is_none() => first_reusable = Some(idx),
                    // OCCUPIED-other / CLAIMING / already-noted TOMBSTONE →
                    // probe past.
                    _ => {}
                }
                idx = self.next_index(idx);
            }
            // Probed every slot without hitting an EMPTY. If a tombstone
            // was seen, recycle it; otherwise the table is saturated.
            match first_reusable {
                Some(target) if self.try_claim(target, root) => {
                    return &self.slots[target].cell;
                }
                Some(_) => {} // lost the race → retry the whole probe
                None => panic!(
                    "settlement table saturated ({} slots); resize / cold-overflow is unimplemented \
                     (iamacoffeepot/aether#1059 spike)",
                    self.slots.len()
                ),
            }
        }
    }

    /// CAS `slots[idx]` from a reusable state (`EMPTY`/`TOMBSTONE`) into
    /// `CLAIMING`, then publish `root`. Returns `true` on success; `false`
    /// if the slot was no longer reusable (another key won it).
    #[inline]
    fn try_claim(&self, idx: usize, root: MailId) -> bool {
        let slot = &self.slots[idx];
        let cur = slot.sv.load(Ordering::Acquire);
        let state = cur & STATE_MASK;
        if state != STATE_EMPTY && state != STATE_TOMBSTONE {
            return false;
        }
        let claiming = with_state(cur, STATE_CLAIMING);
        if slot
            .sv
            .compare_exchange(cur, claiming, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            Self::publish(slot, claiming, root);
            true
        } else {
            false
        }
    }

    /// Find the slot for an *existing* root without inserting. `None` if
    /// the key is absent (probe hit an `EMPTY`).
    #[inline]
    fn find_slot(&self, root: MailId) -> Option<&Slot> {
        let key = (root.sender.0, root.correlation_id);
        let mut idx = self.home(root);
        for _ in 0..=self.mask {
            let slot = &self.slots[idx];
            match slot.sv.load(Ordering::Acquire) & STATE_MASK {
                STATE_OCCUPIED if Self::read_key(slot) == Some(key) => return Some(slot),
                STATE_EMPTY => return None,
                // OCCUPIED-other / TOMBSTONE / CLAIMING → keep probing.
                _ => {}
            }
            idx = self.next_index(idx);
        }
        None
    }

    /// Tombstone `slot` after its count has reached `(0,0)`. Only the
    /// settling decrement's thread calls this, and no claim targets an
    /// `OCCUPIED` slot, so a plain `Release` store is race-free.
    #[inline]
    fn tombstone(slot: &Slot) {
        let cur = slot.sv.load(Ordering::Acquire);
        slot.sv
            .store(with_state(cur, STATE_TOMBSTONE), Ordering::Release);
        // Early warning (debug-only, best-effort): the settling decrement
        // observed `(0,0)`; a non-zero cell now means a `record_sent`
        // re-opened the root in the window before this tombstone. The
        // deterministic guard is the `(0,0)` assert at claim time in
        // `publish`; this just surfaces a violation closer to its source
        // under test.
        debug_assert_eq!(
            slot.cell.load(),
            (0, 0),
            "settlement table: cell re-opened during tombstone (concurrent re-open; \
             see module Contract)"
        );
    }

    /// Test-only: tombstone `root`'s slot while its cell is still live,
    /// simulating the corruption a concurrent re-open would cause. Sets the
    /// state word directly, bypassing [`Self::tombstone`]'s own debug
    /// re-check (which would fire first) — the point is to leave a
    /// live-celled tombstone for the claim-time guard to catch.
    #[cfg(test)]
    fn force_tombstone_live_for_test(&self, root: MailId) {
        let slot = self.find_slot(root).expect("root must be live");
        let cur = slot.sv.load(Ordering::Acquire);
        slot.sv
            .store(with_state(cur, STATE_TOMBSTONE), Ordering::Release);
    }

    /// Record a `Sent` for `root` (`in_flight += 1`). Inserts the slot on
    /// first event.
    pub fn record_sent(&self, root: MailId) {
        self.cell_for(root).add_in_flight();
    }

    /// Record a settlement `HoldOpen` for `root` (`held_open += 1`).
    pub fn record_hold_open(&self, root: MailId) {
        self.cell_for(root).add_held_open();
    }

    /// Record a `Finished` for `root` (`in_flight -= 1`). Returns `true`
    /// iff the root just reached `(0,0)`; tombstones the slot on that
    /// transition. An orphan `Finished` (no live slot) returns `false`.
    #[must_use]
    pub fn record_finished(&self, root: MailId) -> bool {
        let Some(slot) = self.find_slot(root) else {
            return false;
        };
        let settled = slot.cell.sub_in_flight();
        if settled {
            Self::tombstone(slot);
        }
        settled
    }

    /// Record a hold `Release` for `root` (`held_open -= 1`). Returns
    /// `true` iff the root just reached `(0,0)`. Symmetric with
    /// [`Self::record_finished`].
    #[must_use]
    pub fn record_release(&self, root: MailId) -> bool {
        let Some(slot) = self.find_slot(root) else {
            return false;
        };
        let settled = slot.cell.sub_held_open();
        if settled {
            Self::tombstone(slot);
        }
        settled
    }

    /// Number of roots with a live (`OCCUPIED`) slot. For assertions; a
    /// concurrent snapshot, exact only at quiescence.
    #[must_use]
    pub fn live_roots(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.sv.load(Ordering::Acquire) & STATE_MASK == STATE_OCCUPIED)
            .count()
    }

    /// Current `held_open` count for `root` (0 if no live slot).
    #[must_use]
    pub fn held_open(&self, root: MailId) -> u32 {
        self.find_slot(root).map_or(0, |slot| slot.cell.load().1)
    }
}

impl Default for SettlementTable {
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
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
mod tests {
    use super::*;
    use aether_data::MailboxId;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn root(sender: u64, cid: u64) -> MailId {
        MailId {
            sender: MailboxId(sender),
            correlation_id: cid,
        }
    }

    /// Spawn `n` threads each running `body(tid)`, join all. Shared via an
    /// `Arc` so the closure can be `Fn` and capture test state.
    fn run_threads<F: Fn(u64) + Send + Sync + 'static>(n: u64, body: F) {
        let body = Arc::new(body);
        let handles: Vec<_> = (0..n)
            .map(|tid| {
                let body = Arc::clone(&body);
                thread::spawn(move || body(tid))
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    /// Scripted single-root sequence with a re-open: a hold gates the
    /// first settle, then a fresh `Sent` re-opens the same root (its slot
    /// was tombstoned) and settles again. The fire count must be exactly
    /// 2, and the slot must be reclaimed each time.
    #[test]
    fn serial_scripted_fires_and_reclaims() {
        let t = SettlementTable::new();
        let r = root(7, 42);
        let mut fires = 0;

        t.record_sent(r); // (1,0)
        t.record_sent(r); // (2,0)
        t.record_hold_open(r); // (2,1)
        assert!(!t.record_finished(r)); // (1,1)
        assert!(!t.record_release(r)); // (1,0)
        if t.record_finished(r) {
            fires += 1;
        } // (0,0) → fire, tombstone
        assert_eq!(t.live_roots(), 0, "settled root is reclaimed");

        t.record_sent(r); // re-open into a fresh slot (1,0)
        if t.record_finished(r) {
            fires += 1;
        } // (0,0) → fire again

        assert_eq!(fires, 2);
        assert_eq!(t.live_roots(), 0);
    }

    /// Orphan `Finished`/`Release` (no live slot) is a no-op returning
    /// `false`, not a panic — mirrors the incumbent counter's contract.
    #[test]
    fn orphan_decrement_is_noop() {
        let t = SettlementTable::new();
        assert!(!t.record_finished(root(1, 1)));
        assert!(!t.record_release(root(1, 1)));
        assert_eq!(t.live_roots(), 0);
    }

    /// A hold keeps the root open after `in_flight` hits zero; only the
    /// release (with `in_flight` already zero) fires.
    #[test]
    fn hold_gates_settlement() {
        let t = SettlementTable::new();
        let r = root(3, 9);
        t.record_sent(r);
        t.record_hold_open(r);
        assert!(!t.record_finished(r), "in_flight→0 but held_open=1");
        assert_eq!(t.held_open(r), 1);
        assert!(t.record_release(r), "release with in_flight=0 fires");
        assert_eq!(t.live_roots(), 0);
    }

    /// Distinct roots that hash to the same home slot must get distinct
    /// cells — counts never merge, and each settles + reclaims
    /// independently. A tiny table forces the collision + probe chain.
    #[test]
    fn colliding_keys_get_distinct_cells() {
        let t = SettlementTable::with_slots(16);
        // Same sender, correlations differing by multiples of the slot
        // count share the low mask bits → identical home index.
        let a = root(5, 0);
        let b = root(5, 16);
        let c = root(5, 32);
        assert_eq!(t.home(a), t.home(b));
        assert_eq!(t.home(b), t.home(c));

        // Give each a distinct in_flight depth.
        t.record_sent(a);
        t.record_sent(b);
        t.record_sent(b);
        t.record_sent(c);
        t.record_sent(c);
        t.record_sent(c);
        assert_eq!(t.live_roots(), 3);

        // a settles first (depth 1).
        assert!(t.record_finished(a));
        // b needs two finishes.
        assert!(!t.record_finished(b));
        assert!(t.record_finished(b));
        // c needs three.
        assert!(!t.record_finished(c));
        assert!(!t.record_finished(c));
        assert!(t.record_finished(c));

        assert_eq!(t.live_roots(), 0, "all three reclaimed, no merge");
    }

    /// The claim-time invariant guard fires (panics) when a slot is
    /// claimed with a live cell — the signature of a settling decrement
    /// having tombstoned a still-live root (concurrent re-open). Driven
    /// deterministically: force-tombstone a live root, then claim its slot
    /// via a colliding root.
    #[test]
    #[should_panic(expected = "claiming a slot whose cell is non-zero")]
    fn claim_guard_fires_on_tombstoned_live_slot() {
        let t = SettlementTable::with_slots(16);
        let a = root(5, 0);
        // Same sender, correlations differing by a multiple of the slot
        // count share the hash's low bits → identical home slot.
        let b = root(5, 16);
        assert_eq!(t.home(a), t.home(b), "b must collide with a's home slot");

        t.record_sent(a); // a's slot: OCCUPIED, in_flight = 1
        // Corrupt: tombstone a's slot while its cell is still live.
        t.force_tombstone_live_for_test(a);
        // b claims the corrupted tombstone (first reusable on its probe) →
        // the claim-time assert in `publish` sees a non-zero cell and panics.
        t.record_sent(b);
    }

    /// A sliding window of live roots over a small table cycles through
    /// far more roots than slots, exercising tombstone recycling heavily.
    /// Every root must settle exactly once and the table must never
    /// saturate (the window stays well under capacity).
    #[test]
    fn tombstone_recycling_under_sliding_window() {
        let t = SettlementTable::with_slots(16);
        let window = 8u64;
        let total = 5_000u64;
        let mut fires = 0u64;

        for i in 0..total {
            t.record_sent(root(1, i));
            if i >= window && t.record_finished(root(1, i - window)) {
                fires += 1;
            }
        }
        // Drain the final in-flight window.
        for i in total - window..total {
            if t.record_finished(root(1, i)) {
                fires += 1;
            }
        }

        assert_eq!(fires, total, "every root settles exactly once");
        assert_eq!(t.live_roots(), 0);
    }

    /// Contention/backoff-sensitive tests live in `mod heavy`: these lock-free
    /// settlement-table races are timing-sensitive under load, so they are
    /// serialized into the `serial-heavy` nextest group
    /// (`.config/nextest.toml`) to avoid oversubscribing cores against one
    /// another.
    mod heavy {
        use super::*;

        /// The kernel's riskiest property through the full table path: seed
        /// exactly `racers` `in_flight` on one root, then race `racers` threads
        /// each doing one `record_finished`. Exactly one must observe the
        /// zero-arrival, and the slot must reclaim. Repeated to shake out
        /// interleavings.
        #[test]
        fn final_decrement_race_fires_once() {
            for _ in 0..2_000 {
                let t = Arc::new(SettlementTable::new());
                let racers = 4u32;
                let r = root(11, 3);
                for _ in 0..racers {
                    t.record_sent(r);
                }
                let fires = Arc::new(AtomicU32::new(0));
                let start = Arc::new(Barrier::new(racers as usize));
                let mut handles = Vec::new();
                for _ in 0..racers {
                    let t = Arc::clone(&t);
                    let fires = Arc::clone(&fires);
                    let start = Arc::clone(&start);
                    handles.push(thread::spawn(move || {
                        start.wait();
                        if t.record_finished(r) {
                            fires.fetch_add(1, Ordering::Relaxed);
                        }
                    }));
                }
                for h in handles {
                    h.join().unwrap();
                }
                assert_eq!(fires.load(Ordering::Relaxed), 1);
                assert_eq!(t.live_roots(), 0);
            }
        }

        /// Concurrent inc/dec on a *shared* set of roots, modelling fan-out
        /// under live chains: a keepalive unit per root is pre-loaded so
        /// `in_flight` stays at least 1 throughout the concurrent phase (the real
        /// workload never lets a root settle while a send under it can still
        /// race — see the module Contract). No settle/tombstone fires during
        /// the churn; the final drain settles each root exactly once. Tests
        /// concurrent `find` + atomic mutation against the same `OCCUPIED`
        /// slots from many threads.
        #[test]
        fn concurrent_shared_roots_inc_dec_then_drain() {
            let t = Arc::new(SettlementTable::new());
            let roots = 256u64;
            let threads = 8u64;
            let per_root_per_thread = 64u32;

            // Keepalive floor: in_flight >= 1 for every root during the phase.
            for i in 0..roots {
                t.record_sent(root(1, i));
            }

            run_threads(threads, {
                let t = Arc::clone(&t);
                move |tid| {
                    for i in 0..roots {
                        let r = root(1, (i + tid) % roots);
                        for _ in 0..per_root_per_thread {
                            t.record_sent(r);
                            // Never settles: the keepalive holds the floor.
                            assert!(!t.record_finished(r));
                        }
                    }
                }
            });

            // Drain the keepalive: each root settles exactly once.
            let mut fires = 0u64;
            for i in 0..roots {
                if t.record_finished(root(1, i)) {
                    fires += 1;
                }
            }
            assert_eq!(fires, roots, "each root settles exactly once on drain");
            assert_eq!(t.live_roots(), 0);
        }

        /// Concurrent chain births *and* settles across threads, each thread
        /// owning a private stream of distinct roots (so no same-root re-open
        /// — the contract). Thousands of depth-2 chains per thread drive
        /// concurrent claim + tombstone (and probe-chain collisions) on the
        /// shared table. Every chain must settle exactly once and the table
        /// must fully reclaim.
        #[test]
        fn concurrent_distinct_chains_claim_and_reclaim() {
            let t = Arc::new(SettlementTable::new());
            let threads = 8u64;
            let chains_per_thread = 4_000u64;
            let total_fires = Arc::new(AtomicU32::new(0));

            run_threads(threads, {
                let t = Arc::clone(&t);
                let total_fires = Arc::clone(&total_fires);
                move |tid| {
                    for c in 0..chains_per_thread {
                        let r = root(tid + 1, c); // private to this thread
                        t.record_sent(r); // born (1)
                        t.record_sent(r); // child (2)
                        assert!(!t.record_finished(r)); // (1)
                        if t.record_finished(r) {
                            // (0) → settle
                            total_fires.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });

            assert_eq!(
                u64::from(total_fires.load(Ordering::Relaxed)),
                threads * chains_per_thread,
                "every chain settles exactly once"
            );
            assert_eq!(t.live_roots(), 0, "table fully reclaimed");
        }
    }
}
