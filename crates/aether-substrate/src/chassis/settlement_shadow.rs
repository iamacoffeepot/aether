//! ADR-0086 Phase 1 — shadow-mode settlement validation.
//!
//! Wires the emit-time [`SettlementCounter`] alongside the incumbent
//! trace pipeline and cross-checks that the two agree, **without driving
//! the lifecycle** — the `TraceObserverCapability` fold remains the
//! settlement authority until Phase 2 flips frame-gating onto the
//! counter. The point of this phase is to land the (new, concurrent)
//! counting kernel in production paths and prove on real workloads that
//! its zero-transitions match the incumbent's, before anything depends
//! on it.
//!
//! **Off by default.** The apparatus is gated by an [`AtomicBool`] seeded
//! from `AETHER_SETTLEMENT_SHADOW` (`"1"` → on). When off, every producer
//! hook is a single relaxed atomic load and a branch — so a normal
//! substrate (and the perf harness) pays effectively nothing. Tests and
//! shadow-validation runs turn it on via [`ShadowSettlement::set_enabled`]
//! (or the env var) *before* any traffic flows — toggling it mid-run
//! would desync the counter (it would miss the earlier `Sent`s its later
//! `Finished`s balance against).
//!
//! Removed or repurposed in Phase 2: when the counter becomes the
//! authority it fires `fire_settled` directly and the observer's role
//! (and this cross-check) goes away.

use std::collections::HashMap;
use std::env;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use aether_data::MailId;

use super::settlement_counter::SettlementCounter;

/// Self-bounding agreement ledger between the two settlement paths.
///
/// `+1` when the emit-time counter settles a root, `-1` when the
/// observer's authoritative `Settled` is routed (the chassis-router
/// tap). The emit side fires first (synchronously, on the producing
/// thread); the observer side follows once the drainer ships and the
/// fold catches up. So a healthy root's balance goes `+1` then back to
/// `0` within that window, and the entry is reclaimed — the map only
/// ever holds roots currently *inside* the `[emit, observer]` window,
/// never the cumulative history (the self-bounding property; see the
/// `self-bounding-over-keep-and-gc` rule).
///
/// A balance that goes **negative** means the observer settled a root
/// the emit counter did not — a real disagreement (the emit counter
/// under-counted), logged immediately. A balance that **lingers
/// positive** past quiescence means the emit counter settled a root the
/// observer never did; tests assert [`Self::outstanding`] is empty after
/// the work drains to catch that direction.
#[derive(Debug, Default)]
pub struct SettlementCrossCheck {
    balance: Mutex<HashMap<MailId, i64>>,
    disagreements: AtomicU64,
    /// Monotonic count of emit-time / observer settles seen. Exposed so
    /// tests can confirm the apparatus actually ran (a balanced ledger is
    /// otherwise indistinguishable from a disabled, never-touched one).
    emit_settles: AtomicU64,
    observer_settles: AtomicU64,
}

impl SettlementCrossCheck {
    /// Apply `delta` to `root`'s balance, reclaiming the entry when it
    /// returns to zero. Returns the post-update balance.
    fn apply(&self, root: MailId, delta: i64) -> i64 {
        let mut bal = self
            .balance
            .lock()
            .expect("settlement cross-check mutex poisoned; fail-fast per ADR-0063");
        let entry = bal.entry(root).or_insert(0);
        *entry += delta;
        let post = *entry;
        if post == 0 {
            bal.remove(&root);
        }
        post
    }

    /// Record an emit-time settle for `root`.
    pub fn note_emit(&self, root: MailId) {
        self.emit_settles.fetch_add(1, Ordering::Relaxed);
        self.apply(root, 1);
    }

    /// Record the observer's authoritative settle for `root`. A balance
    /// that drops below zero is a disagreement (the observer settled a
    /// root the emit counter never did).
    pub fn note_observer(&self, root: MailId) {
        self.observer_settles.fetch_add(1, Ordering::Relaxed);
        if self.apply(root, -1) < 0 {
            self.disagreements.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                target: "aether_substrate::settlement_shadow",
                ?root,
                "settlement shadow disagreement: observer settled a root the emit-time \
                 counter did not (ADR-0086 Phase 1 cross-check)"
            );
        }
    }

    /// Count of disagreements detected so far (observer-settled roots the
    /// emit counter missed). Tests assert this is zero.
    #[must_use]
    pub fn disagreements(&self) -> u64 {
        self.disagreements.load(Ordering::Relaxed)
    }

    /// Total emit-time settles recorded (monotonic). Lets a test confirm
    /// the emit path actually ran rather than passing vacuously.
    #[must_use]
    pub fn emit_settles(&self) -> u64 {
        self.emit_settles.load(Ordering::Relaxed)
    }

    /// Total observer settles recorded (monotonic).
    #[must_use]
    pub fn observer_settles(&self) -> u64 {
        self.observer_settles.load(Ordering::Relaxed)
    }

    /// Roots whose balance is currently non-zero — i.e. settled by one
    /// path but not (yet) the other. After the workload quiesces (the
    /// drainer has flushed and the observer has caught up) this must be
    /// empty; a lingering entry is a disagreement in the
    /// emit-settled-but-observer-didn't direction.
    ///
    /// # Panics
    /// Panics on a poisoned ledger mutex (fail-fast per ADR-0063).
    #[must_use]
    pub fn outstanding(&self) -> Vec<(MailId, i64)> {
        self.balance
            .lock()
            .expect("settlement cross-check mutex poisoned; fail-fast per ADR-0063")
            .iter()
            .map(|(&root, &bal)| (root, bal))
            .collect()
    }
}

/// Reads the `AETHER_SETTLEMENT_SHADOW` env var: shadow on iff it is
/// exactly `"1"`.
fn shadow_enabled_from_env() -> bool {
    env::var("AETHER_SETTLEMENT_SHADOW").is_ok_and(|v| v == "1")
}

/// The Phase-1 shadow apparatus carried by the chassis trace handle: the
/// emit-time [`SettlementCounter`], the [`SettlementCrossCheck`] against
/// the observer, and an enable flag.
///
/// Cloned cheaply via the `Arc` the trace handle holds; every producer
/// hook routes through one instance per chassis.
#[derive(Debug)]
pub struct ShadowSettlement {
    enabled: AtomicBool,
    counter: SettlementCounter,
    cross_check: SettlementCrossCheck,
}

impl ShadowSettlement {
    /// Construct with the enable flag seeded from
    /// `AETHER_SETTLEMENT_SHADOW`.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            enabled: AtomicBool::new(shadow_enabled_from_env()),
            counter: SettlementCounter::new(),
            cross_check: SettlementCrossCheck::default(),
        }
    }

    /// Enable or disable the shadow apparatus. Call **before** any
    /// traffic — flipping it on mid-run desyncs the counter (see the
    /// module docs).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Whether the shadow apparatus is currently active.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The agreement ledger, for test assertions.
    #[must_use]
    pub fn cross_check(&self) -> &SettlementCrossCheck {
        &self.cross_check
    }

    /// True iff the apparatus is active and `root` is a real root.
    /// `MailId::NONE` is the chassis-root / recursion-break sentinel and
    /// never carries settlement accounting.
    #[inline]
    fn active(&self, root: MailId) -> bool {
        self.is_enabled() && root != MailId::NONE
    }

    /// Emit-side hook for a `Sent`: `in_flight += 1`.
    pub fn on_sent(&self, root: MailId) {
        if self.active(root) {
            self.counter.record_sent(root);
        }
    }

    /// Emit-side hook for a settlement `HoldOpen`: `held_open += 1`.
    pub fn on_hold_open(&self, root: MailId) {
        if self.active(root) {
            self.counter.record_hold_open(root);
        }
    }

    /// Emit-side hook for a `Finished`: `in_flight -= 1`. On the
    /// zero-transition, note the emit-time settle to the cross-check.
    pub fn on_finished(&self, root: MailId) {
        if self.active(root) && self.counter.record_finished(root) {
            self.cross_check.note_emit(root);
        }
    }

    /// Emit-side hook for a hold `Release`: `held_open -= 1`. On the
    /// zero-transition, note the emit-time settle.
    pub fn on_release(&self, root: MailId) {
        if self.active(root) && self.counter.record_release(root) {
            self.cross_check.note_emit(root);
        }
    }

    /// Observer-side hook: the authoritative `Settled { root }` was just
    /// routed (chassis-router). Note it against the emit-time settle.
    pub fn on_observer_settled(&self, root: MailId) {
        if self.active(root) {
            self.cross_check.note_observer(root);
        }
    }
}

impl Default for ShadowSettlement {
    fn default() -> Self {
        Self::from_env()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::MailboxId;

    fn root(sender: u64, cid: u64) -> MailId {
        MailId {
            sender: MailboxId(sender),
            correlation_id: cid,
        }
    }

    #[test]
    fn cross_check_balances_and_reclaims() {
        let xc = SettlementCrossCheck::default();
        let r = root(1, 1);
        xc.note_emit(r);
        assert_eq!(xc.outstanding(), vec![(r, 1)]);
        xc.note_observer(r);
        assert!(
            xc.outstanding().is_empty(),
            "matched emit+observer reclaims the entry"
        );
        assert_eq!(xc.disagreements(), 0);
    }

    #[test]
    fn cross_check_flags_observer_only_settle() {
        let xc = SettlementCrossCheck::default();
        let r = root(1, 2);
        // Observer settles a root the emit counter never did.
        xc.note_observer(r);
        assert_eq!(xc.disagreements(), 1);
    }

    #[test]
    fn cross_check_handles_reopen_multiplicity() {
        // A root that settles twice (re-open) must net to zero across two
        // emit + two observer notes, in any interleaving.
        let xc = SettlementCrossCheck::default();
        let r = root(1, 3);
        xc.note_emit(r);
        xc.note_emit(r);
        xc.note_observer(r);
        xc.note_observer(r);
        assert!(xc.outstanding().is_empty());
        assert_eq!(xc.disagreements(), 0);
    }

    #[test]
    fn disabled_shadow_is_inert() {
        let s = ShadowSettlement::from_env();
        s.set_enabled(false);
        let r = root(1, 4);
        s.on_sent(r);
        s.on_finished(r);
        // Nothing recorded — the cross-check never saw an emit.
        assert!(s.cross_check().outstanding().is_empty());
        assert_eq!(s.cross_check().disagreements(), 0);
    }

    #[test]
    fn enabled_shadow_records_emit_settle() {
        let s = ShadowSettlement::from_env();
        s.set_enabled(true);
        let r = root(1, 5);
        s.on_sent(r);
        s.on_finished(r); // in_flight 1 -> 0 -> emit settle noted
        assert_eq!(s.cross_check().outstanding(), vec![(r, 1)]);
        s.on_observer_settled(r);
        assert!(s.cross_check().outstanding().is_empty());
        assert_eq!(s.cross_check().disagreements(), 0);
    }

    #[test]
    fn none_root_is_ignored() {
        let s = ShadowSettlement::from_env();
        s.set_enabled(true);
        // The recursion-break sentinel carries no accounting.
        s.on_sent(MailId::NONE);
        s.on_finished(MailId::NONE);
        assert!(s.cross_check().outstanding().is_empty());
    }
}
