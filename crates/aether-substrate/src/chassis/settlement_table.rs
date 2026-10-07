//! Lock-free open-addressing settlement table — the production settlement
//! authority (ADR-0080 / ADR-0086).
//!
//! [`super::settlement_counter::SettlementCounter`] guards its
//! `root -> CounterCell` map with a striped `Mutex`; the producer harness
//! (`settlement_counter::tests::bench_producer_hot_path`) measures
//! ~67 ns per `Sent`+`Finished` pair under that lock versus ~4 ns on the
//! bare [`CounterCell`] atomic. The lock guards only the *map structure*
//! (insert-on-first-event, drop-on-settle); the per-root count is already
//! a lock-free atomic word. This table makes the map itself lock-free, so
//! every unspilled `record_*` call is a probe plus a bare atomic — the ~4 ns
//! floor on every access, not just a cached one — with a gated cold
//! spillover absorbing peak concurrent live roots beyond the fast-path
//! slots (see below).
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
//! clean `TOMBSTONE -> CLAIMING -> OCCUPIED` claim path. Soundness beyond
//! this table rests on that invariant holding on every settlement path;
//! re-open robustness (a linked decrement-and-tombstone) is the genuinely
//! hard lock-free piece, deliberately not built.
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
//! (self-bounding per ADR-0086). A full probe sweep with no reusable slot
//! does not panic: the root spills into the cold overflow instead (see
//! below). `DEFAULT_SLOTS` therefore sizes the lock-free fast path, while
//! peak concurrent live roots beyond it spill instead of panicking —
//! capacity is no longer a correctness boundary.
//!
//! **Gated cold spillover.** The overflow is a `HashMap<MailId, CounterCell>`
//! under a poison-fail-fast `Mutex`, holding the excess roots'
//! `CounterCell`s from spill until they settle; `spilled` counts the roots
//! currently resident there. Every path that would otherwise lock reads the
//! gate first: a `spilled` load of zero skips the mutex entirely, so no lock
//! is taken while nothing has spilled. The overflow mutex is taken only
//! while roots are spilled (never on the unspilled hot path, per the gate),
//! and the `spilled` atomic costs one `Acquire` load per table miss. The
//! insert raises the gate with `Release` and stores the map entry under the
//! mutex; the settle removes the entry and lowers the gate with `Release`
//! under the same mutex, so the gate is never below the overflow length; every gated path reads the gate with
//! `Acquire`, so the decision to skip the lock happens-after the insert it
//! might otherwise miss. A stale-high gate only costs a useless lock, never
//! correctness; the full-sweep insert locks unconditionally rather than
//! consulting the gate, so a stale gate read never skips the authoritative
//! check.
//!
//! **Why the mutex and the atomic do not belong to an actor.** The threads
//! touching them are every thread that records settlement — dispatcher
//! workers, producers sending through the handle hooks, offload workers,
//! pumped-slot drivers, harness threads — arbitrary producer threads with
//! no single owning actor across a fan-out that spans many actors.
//! Settlement accounting is the convergence point below the actor layer, the
//! same grain as the table's own atomics. Funneling each accounting event
//! through an owning actor's mailbox would put settlement back on a mail hop
//! on the frame's critical path and serialize wide fan-out accounting
//! through one actor — the design ADR-0086 §Decision 1 explicitly rejected
//! in favor of atomics.
//!
//! **Probe bound (iamacoffeepot/aether#7126).** Because slots never return
//! to `EMPTY`, churn eventually leaves no `EMPTY` anywhere, and a probe that
//! could only stop at one would walk the whole table for every absent key —
//! every new root, and every `is_live` on a settled one — for the rest of
//! the process. So the table records `max_probe`, the furthest from its home
//! slot any insert has landed, and every lookup stops there. An insert
//! raises the bound *before* it publishes its key, and a lookup of a present
//! key happens-after that key's insert (invariant 2: the looker holds
//! in-flight work under the root, which the minting `record_sent` preceded),
//! so the lookup's `Acquire` load sees a bound that reaches the key. The
//! bound only grows, and it tracks the peak clustering of *live* roots, not
//! the number of roots the table has ever held.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Mutex, MutexGuard};

use aether_data::{MailId, MailboxId};

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
/// stay near length 1. Peak concurrent live roots beyond this spill into
/// the cold overflow instead of panicking.
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
    /// Key hi: the root's `sender` [`MailboxId`] word.
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
/// `live_roots` / `held_open` surface, no lock on any unspilled path.
#[derive(Debug)]
pub struct SettlementTable {
    slots: Box<[Slot]>,
    mask: u64,
    /// The furthest any insert has landed from its home slot, in slots.
    /// Every lookup probes at most `max_probe + 1` slots (see the module's
    /// probe-bound note). Only ever grows.
    max_probe: AtomicUsize,
    /// Cold spillover for roots beyond the fast-path slots: the excess
    /// roots' `CounterCell`s, insert-on-spill and drop-on-settle. Taken
    /// only while roots are spilled (never on the unspilled hot path —
    /// every gated path skips the mutex on a zero `spilled` load).
    overflow: Mutex<HashMap<MailId, CounterCell>>,
    /// Roots currently resident in `overflow`. Raised under the overflow
    /// lock on insert, lowered under the same lock on removal, so it is never
    /// below the overflow length; gated paths read it first with `Acquire`
    /// and skip the mutex entirely on zero.
    spilled: AtomicUsize,
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
            max_probe: AtomicUsize::new(0),
            overflow: Mutex::new(HashMap::new()),
            spilled: AtomicUsize::new(0),
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
        fence(Ordering::Acquire);
        let sv2 = slot.sv.load(Ordering::Relaxed);
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
        slot.correlation.store(root.correlation_id, Ordering::Relaxed);
        slot.cell.reset();
        slot.sv.store((claiming_sv & !STATE_MASK) | STATE_OCCUPIED, Ordering::Release);
    }

    /// Claim `root` in the table only, inserting if absent. Returns `None`
    /// on a full sweep (every slot live or claiming) so the caller can
    /// spill into the cold overflow instead of panicking. Never returns a
    /// borrow tied to anything but `&self` — slots live for the table's
    /// lifetime and never move, so the reference stays valid through any
    /// concurrent reclaim of *other* slots.
    fn try_table_cell(&self, root: MailId) -> Option<&CounterCell> {
        let home = self.home(root);
        loop {
            let (target, distance) = self.first_reusable(home)?;
            // Raise the bound before the key is published, so every lookup
            // that happens-after the publish probes far enough to find it.
            self.max_probe.fetch_max(distance, Ordering::Release);
            if self.try_claim(target, root) {
                return Some(&self.slots[target].cell);
            }
            // Lost the slot to another key's claim → re-probe.
        }
    }

    /// Lock the cold overflow, poison-fail-fast per ADR-0063 (same contract
    /// as the striped counter's stripe lock).
    #[inline]
    fn lock_overflow(&self) -> MutexGuard<'_, HashMap<MailId, CounterCell>> {
        self.overflow.lock().expect("settlement table overflow mutex poisoned; fail-fast per ADR-0063")
    }

    /// Whether any root is spilled. Every path that would otherwise lock the
    /// overflow reads this first and skips the mutex when it is `false`.
    /// `Acquire` pairs with the `Release` updates in [`Self::spill`] and
    /// [`Self::lower_spilled`], so a thread that learned of a spilled root
    /// from the thread that spilled it also sees the raised count.
    #[inline]
    fn any_spilled(&self) -> bool {
        self.spilled.load(Ordering::Acquire) != 0
    }

    /// Spill `root` into the overflow and apply `step` to its cell, under the
    /// overflow lock. Called only after a full table sweep. It locks without
    /// reading `spilled` first, because a stale read must never skip the
    /// authoritative check. The count is raised before the entry is inserted,
    /// so it is never below the overflow length.
    fn spill(&self, root: MailId, step: impl Fn(&CounterCell)) {
        let mut overflow = self.lock_overflow();
        match overflow.entry(root) {
            Entry::Occupied(entry) => step(entry.get()),
            Entry::Vacant(entry) => {
                self.spilled.fetch_add(1, Ordering::Release);
                step(entry.insert(CounterCell::zero()));
            }
        }
    }

    /// Apply `step` to `root`'s overflow cell under the overflow lock.
    /// `false` when the overflow holds no such root.
    fn raise_spilled(&self, root: MailId, step: impl Fn(&CounterCell)) -> bool {
        let overflow = self.lock_overflow();
        let cell = overflow.get(&root);
        if let Some(cell) = cell {
            step(cell);
        }
        let counted = cell.is_some();
        drop(overflow);
        counted
    }

    /// Apply the decrement `step` to `root`'s overflow cell under the overflow
    /// lock, and remove the entry under the same lock when it settles.
    /// Returns whether the root settled; `false` when the overflow holds no
    /// such root, which is the orphan decrement.
    fn lower_spilled(&self, root: MailId, step: impl Fn(&CounterCell) -> bool) -> bool {
        let mut overflow = self.lock_overflow();
        let Some(cell) = overflow.get(&root) else {
            return false;
        };
        let settled = step(cell);
        if settled {
            overflow.remove(&root);
            self.spilled.fetch_sub(1, Ordering::Release);
        }
        // The count is lowered before the lock is released, so it is never
        // below the overflow length.
        drop(overflow);
        settled
    }

    /// The first `EMPTY` or `TOMBSTONE` slot probing from `home`, with its
    /// distance from `home`. Unbounded by `max_probe`: this is where the
    /// bound gets extended. `None` only when every slot is live or claiming.
    fn first_reusable(&self, home: usize) -> Option<(usize, usize)> {
        let mut idx = home;
        for distance in 0..self.slots.len() {
            let state = self.slots[idx].sv.load(Ordering::Acquire) & STATE_MASK;
            if state == STATE_EMPTY || state == STATE_TOMBSTONE {
                return Some((idx, distance));
            }
            idx = self.next_index(idx);
        }
        None
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
        if slot.sv.compare_exchange(cur, claiming, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            Self::publish(slot, claiming, root);
            true
        } else {
            false
        }
    }

    /// Find the slot for an *existing* root without inserting. `None` if
    /// the key is absent (probe hit an `EMPTY` or ran past `max_probe`).
    #[inline]
    fn find_slot(&self, root: MailId) -> Option<&Slot> {
        let key = (root.sender.0, root.correlation_id);
        let mut idx = self.home(root);
        for _ in 0..=self.max_probe.load(Ordering::Acquire) {
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
        slot.sv.store(with_state(cur, STATE_TOMBSTONE), Ordering::Release);
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
        slot.sv.store(with_state(cur, STATE_TOMBSTONE), Ordering::Release);
    }

    /// Record a `Sent` for `root` (`in_flight += 1`). Inserts the slot on
    /// first event, spilling into the cold overflow on a full sweep.
    pub fn record_sent(&self, root: MailId) {
        self.raise(root, CounterCell::add_in_flight);
    }

    /// Record a settlement `HoldOpen` for `root` (`held_open += 1`).
    /// Symmetric with [`Self::record_sent`].
    pub fn record_hold_open(&self, root: MailId) {
        self.raise(root, CounterCell::add_held_open);
    }

    /// Apply the increment `step` to `root`'s cell, wherever the root lives.
    /// A root already in the table is counted there with no lock. Otherwise
    /// the overflow is checked before any table claim, and only while roots
    /// are spilled, so a spilled root is never given a second cell in the
    /// table. A root in neither store claims a table slot, or spills when the
    /// table is full.
    fn raise(&self, root: MailId, step: impl Fn(&CounterCell)) {
        if let Some(slot) = self.find_slot(root) {
            step(&slot.cell);
            return;
        }

        if self.any_spilled() {
            let counted = self.raise_spilled(root, &step);
            if counted {
                return;
            }
        }

        match self.try_table_cell(root) {
            Some(cell) => step(cell),
            None => self.spill(root, step),
        }
    }

    /// Record a `Finished` for `root` (`in_flight -= 1`). Returns `true`
    /// iff the root just reached `(0,0)`; tombstones the slot on that
    /// transition, or removes the overflow entry under the overflow lock.
    /// An orphan `Finished` (no live slot, no overflow entry) returns
    /// `false`.
    #[must_use]
    pub fn record_finished(&self, root: MailId) -> bool {
        self.lower(root, CounterCell::sub_in_flight)
    }

    /// Record a hold `Release` for `root` (`held_open -= 1`). Returns
    /// `true` iff the root just reached `(0,0)`. Symmetric with
    /// [`Self::record_finished`].
    #[must_use]
    pub fn record_release(&self, root: MailId) -> bool {
        self.lower(root, CounterCell::sub_held_open)
    }

    /// Apply the decrement `step` to `root`'s cell, wherever the root lives,
    /// and reclaim its place when it settles. With nothing spilled, a root
    /// absent from the table is an orphan and no lock is taken.
    fn lower(&self, root: MailId, step: impl Fn(&CounterCell) -> bool) -> bool {
        if let Some(slot) = self.find_slot(root) {
            let settled = step(&slot.cell);
            if settled {
                Self::tombstone(slot);
            }
            return settled;
        }

        self.any_spilled() && self.lower_spilled(root, step)
    }

    /// Number of live roots across both stores. For assertions; a
    /// concurrent snapshot, exact only at quiescence. Sums the table
    /// `OCCUPIED` count and the `spilled` gate (the overflow length once no
    /// insert or removal is in progress) and never locks.
    #[must_use]
    pub fn live_roots(&self) -> usize {
        let table_live =
            self.slots.iter().filter(|s| s.sv.load(Ordering::Acquire) & STATE_MASK == STATE_OCCUPIED).count();
        let spilled = self.spilled.load(Ordering::Acquire);
        table_live + spilled
    }

    /// Current `held_open` count for `root` (0 if no live slot and no
    /// overflow entry).
    #[must_use]
    pub fn held_open(&self, root: MailId) -> u32 {
        if let Some(slot) = self.find_slot(root) {
            return slot.cell.load().1;
        }

        if !self.any_spilled() {
            return 0;
        }
        self.lock_overflow().get(&root).map_or(0, |cell| cell.load().1)
    }

    /// Whether `root` is still live in either store — `in_flight > 0` or
    /// `held_open > 0`. The trace ring's eviction hint (issue 2076): a
    /// settled root is tombstoned, so `is_live == false` means the ring
    /// may reclaim that root's oldest entry instead of growing to retain
    /// it; only a still-live (in-flight) oldest chain forces growth. O(1)
    /// open-addressing probe, the same lookup `record_finished` does. A
    /// concurrent read is best-effort: a stale answer only costs a
    /// slightly-early grow or slightly-late reclaim, never correctness.
    #[must_use]
    pub fn is_live(&self, root: MailId) -> bool {
        if self.find_slot(root).is_some() {
            return true;
        }

        self.any_spilled() && self.lock_overflow().contains_key(&root)
    }

    /// Snapshot every live root and its `(in_flight, held_open)` counts —
    /// the diagnostic surface a wedged settlement gate dumps so a genuine
    /// deadlock/livelock names its stuck roots instead of surfacing a bare
    /// timeout (issue 2062). Chains the table snapshot with the overflow
    /// entries (locking only on a nonzero gate): a concurrent snapshot,
    /// exact only at quiescence and best-effort under churn (a slot whose
    /// occupant changes mid-read fails the seqlock and is skipped). A root
    /// lives in exactly one store at a time, so the chain never double
    /// counts.
    #[must_use]
    pub fn pending_roots(&self) -> Vec<(MailId, u32, u32)> {
        let mut pending: Vec<(MailId, u32, u32)> = self
            .slots
            .iter()
            .filter_map(|slot| {
                let (sender, correlation) = Self::read_key(slot)?;
                let (in_flight, held_open) = slot.cell.load();
                Some((MailId { sender: MailboxId(sender), correlation_id: correlation }, in_flight, held_open))
            })
            .collect();

        if self.any_spilled() {
            pending.extend(self.lock_overflow().iter().map(|(root, cell)| {
                let (in_flight, held_open) = cell.load();
                (*root, in_flight, held_open)
            }));
        }
        pending
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
        MailId { sender: MailboxId(sender), correlation_id: cid }
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

    /// `pending_roots` enumerates every live root with its
    /// `(in_flight, held_open)` counts — the wedge-diagnostic surface
    /// (issue 2062). Two roots with distinct counts show up; the table is
    /// empty once both settle.
    #[test]
    fn pending_roots_enumerates_live_counts() {
        let t = SettlementTable::new();
        let a = root(1, 1);
        let b = root(2, 2);
        t.record_sent(a); // a: (1,0)
        t.record_sent(a); // a: (2,0)
        t.record_sent(b); // b: (1,0)
        t.record_hold_open(b); // b: (1,1)

        let mut pending = t.pending_roots();
        pending.sort_by_key(|(r, _, _)| (r.sender.0, r.correlation_id));
        assert_eq!(pending, vec![(a, 2, 0), (b, 1, 1)]);

        // Drain both to (0,0); the table empties.
        assert!(!t.record_finished(a)); // (1,0)
        assert!(t.record_finished(a)); // (0,0)
        assert!(!t.record_finished(b)); // (0,1)
        assert!(t.record_release(b)); // (0,0)
        assert!(t.pending_roots().is_empty(), "settled roots leave no pending entries");
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

    /// `is_live` (issue 2076, the trace ring's eviction hint) tracks the
    /// slot's lifetime: false before any event, true while in-flight or
    /// held-open, false again once settled (tombstoned).
    #[test]
    fn is_live_tracks_slot_lifetime() {
        let t = SettlementTable::new();
        let r = root(7, 7);
        assert!(!t.is_live(r), "untracked root is not live");
        t.record_sent(r);
        assert!(t.is_live(r), "in-flight root is live");
        t.record_hold_open(r);
        assert!(!t.record_finished(r), "in_flight→0 but held_open=1");
        assert!(t.is_live(r), "held-open root is still live");
        assert!(t.record_release(r), "release fires settlement");
        assert!(!t.is_live(r), "settled root is no longer live");
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

    /// The probe bound follows *live* roots, not churn (iamacoffeepot/aether#7126).
    /// Thousands of roots through a 16-slot table leave no `EMPTY` slot, but
    /// with at most `window` roots live at any insert, the first reusable
    /// slot is never more than `window` probes from home — so the bound stays
    /// there. A bound fed by the distance to the next `EMPTY` (which grows to
    /// the whole table under churn) instead of the claimed slot fails this,
    /// and so does one never raised: a live root past the bound would go
    /// unfound and never settle.
    #[test]
    fn probe_bound_tracks_live_roots_not_churn() {
        let t = SettlementTable::with_slots(16);
        let window = 2u64;
        let total = 5_000u64;
        let mut fires = 0u64;

        for i in 0..total {
            t.record_sent(root(1, i));
            if i >= window && t.record_finished(root(1, i - window)) {
                fires += 1;
            }
        }
        for i in total - window..total {
            if t.record_finished(root(1, i)) {
                fires += 1;
            }
        }

        assert_eq!(fires, total, "every root settles exactly once");
        assert!(
            (0..16).all(|i| t.slots[i].sv.load(Ordering::Acquire) & STATE_MASK != STATE_EMPTY),
            "the churn used every slot, so no lookup can stop at an EMPTY"
        );
        let bound = t.max_probe.load(Ordering::Acquire);
        assert!(bound <= usize::try_from(window).unwrap(), "probe bound {bound} grew past the live window {window}");
        assert!(!t.is_live(root(1, 0)), "a settled root reads as not live within the bound");
    }

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

    /// More concurrent live roots than a small table's slots must spill
    /// into the cold overflow instead of panicking, then settle exactly
    /// once with full reclaim. Catches the reported saturation panic.
    #[test]
    fn overflow_absorbs_beyond_capacity_and_settles_exactly_once() {
        let t = SettlementTable::with_slots(16);
        let total = 64u64;

        for i in 0..total {
            t.record_sent(root(1, i));
        }

        let spilled = t.spilled.load(Ordering::Acquire);
        assert!(spilled > 0, "peak {total} over 16 slots must spill, spilled={spilled}");
        assert_eq!(t.live_roots(), usize::try_from(total).unwrap());
        assert_eq!(t.pending_roots().len(), usize::try_from(total).unwrap());
        assert!(t.is_live(root(1, 0)));
        assert!(t.is_live(root(1, total - 1)));

        let mut fires = 0u64;
        for i in 0..total {
            if t.record_finished(root(1, i)) {
                fires += 1;
            }
        }

        assert_eq!(fires, total, "every root settles exactly once");
        assert_eq!(t.live_roots(), 0);
        assert!(t.pending_roots().is_empty());
        assert_eq!(t.spilled.load(Ordering::Acquire), 0, "overflow fully drained");
        assert_eq!(t.lock_overflow().len(), 0, "overflow map fully reclaimed");
    }

    /// A sliding settle window wider than the table forces table recycling
    /// and overflow residency to interleave: spilled roots settle while
    /// table slots free and recycle around them. Catches a root
    /// double-counted across both stores (which would leak or misfire).
    #[test]
    fn overflow_interleaves_with_table_recycling() {
        let t = SettlementTable::with_slots(16);
        let window = 32u64;
        let total = 500u64;
        let mut fires = 0u64;

        for i in 0..total {
            t.record_sent(root(1, i));
            if i >= window {
                let old = root(1, i - window);
                if t.record_finished(old) {
                    fires += 1;
                }
                assert!(!t.is_live(old), "settled root leaves both stores");
            }
        }
        for i in total - window..total {
            if t.record_finished(root(1, i)) {
                fires += 1;
            }
        }

        assert_eq!(fires, total, "every root settles exactly once");
        assert_eq!(t.live_roots(), 0);
        assert!(t.pending_roots().is_empty(), "no root counted in either store after drain");
        assert_eq!(t.spilled.load(Ordering::Acquire), 0, "overflow fully drained");
    }

    /// Concurrent private per-thread root streams through a tiny table race
    /// overflow inserts and settle-removals on the shared cold mutex.
    /// Catches a decrement routed to the wrong store under contention
    /// (which would miss its fire or leak a live root).
    #[test]
    fn overflow_drains_exactly_once_under_threads() {
        let t = Arc::new(SettlementTable::with_slots(16));
        let threads = 8u64;
        let per_thread = 200u64;
        let total_fires = Arc::new(AtomicU32::new(0));

        run_threads(threads, {
            let t = Arc::clone(&t);
            let total_fires = Arc::clone(&total_fires);
            move |tid| {
                for c in 0..per_thread {
                    t.record_sent(root(tid + 1, c));
                }
                for c in 0..per_thread {
                    if t.record_finished(root(tid + 1, c)) {
                        total_fires.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });

        assert_eq!(
            u64::from(total_fires.load(Ordering::Relaxed)),
            threads * per_thread,
            "every spilled chain settles exactly once"
        );
        assert_eq!(t.live_roots(), 0, "table plus spillover fully reclaimed");
        assert!(t.pending_roots().is_empty());
        assert_eq!(t.spilled.load(Ordering::Acquire), 0, "overflow fully drained");
    }

    /// No unspilled path takes the overflow mutex. The test holds the
    /// private overflow lock while a worker drives the full eight-method
    /// workload over hundreds of chains that never spill; completion (over
    /// a generous multi-second rendezvous that turns a mutex-block hang
    /// into a failure naming the gate) proves every gated path skipped the
    /// lock on its zero gate. Catches a regression to an unconditional
    /// overflow lock on the hot path.
    #[test]
    fn unspilled_paths_skip_overflow_mutex() {
        use std::sync::mpsc;
        use std::time::Duration;

        let t = Arc::new(SettlementTable::with_slots(64));
        let guard = t.overflow.try_lock().unwrap();
        let (done_tx, done_rx) = mpsc::channel();

        let worker = Arc::clone(&t);
        thread::spawn(move || {
            for c in 0..300u64 {
                let r = root(99, c);
                worker.record_sent(r);
                worker.record_hold_open(r);
                assert!(worker.is_live(r));
                assert_eq!(worker.held_open(r), 1);
                let _ = worker.live_roots();
                let _ = worker.pending_roots();
                assert!(!worker.record_finished(r));
                assert!(worker.is_live(r));
                assert!(worker.record_release(r));
                assert!(!worker.is_live(r));
            }
            let _ = done_tx.send(());
        });

        let completed = done_rx.recv_timeout(Duration::from_secs(10)).is_ok();
        assert!(
            completed,
            "unspilled workload blocked on the overflow mutex: gated paths must skip the lock on a zero gate"
        );
        drop(guard);
        assert_eq!(t.live_roots(), 0);
        assert!(t.pending_roots().is_empty());
        assert_eq!(t.spilled.load(Ordering::Acquire), 0);
    }
}
