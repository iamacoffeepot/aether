//! ADR-0080 §6 settlement registry — chassis-side gate-notification
//! map for `Settled { root }` mail.
//!
//! Two subscriber shapes share one pending map (keyed on root
//! [`MailId`]):
//!
//! - [`SettlementRegistry::subscribe_settlement`] returns a
//!   `crossbeam_channel::Receiver<()>` for in-thread waiters
//!   (chassis-internal code, tests) that can block on `recv` directly.
//! - [`SettlementRegistry::subscribe_settlement_mail`] pushes a
//!   notification mail to a target mailbox when the root settles —
//!   for actors whose thread is committed to its mpsc inbox and
//!   can't block on a separate channel without per-cid helper threads.
//!
//! Both fire when the [`crate::actor::native`] dispatcher routes a
//! `Settled { root }` mail addressed to
//! [`MailboxId::CHASSIS_MAILBOX_ID`] through the
//! registry's [`SettlementRegistry::fire_settled`] hook.
//!
//! ADR-0080 §6 framing: settlement is eventually-consistent, not
//! transactional. Two races are handled here:
//!
//! - **Subscribe-after-fire.** A gate may subscribe to a root that
//!   already settled (the `Finished` event landed before the gate
//!   site got around to subscribing). The registry tracks
//!   already-fired roots in a small `HashSet`; subscribing to one
//!   pre-fires the receiver immediately so the gate doesn't hang.
//! - **Duplicate `fire_settled`.** Per ADR §6, settlement is a hint
//!   — a root may report settled multiple times under retries or
//!   late-arriving `Finished` events. The registry's `fire_settled`
//!   is idempotent: subsequent calls for the same root after the
//!   subscribers have drained are no-ops (the `HashSet` hit short-
//!   circuits).
//!
//! The `settled` `HashSet` grows unboundedly within a chassis lifetime
//! today. PR 5 (or a later cleanup) wires retention against the
//! observer's eviction policy. For v1 — a chassis runs for a session,
//! not forever — the cap-by-count plus per-process tear-down keeps
//! memory bounded.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use aether_data::{Kind, KindId, MailId, MailboxId};
use aether_kinds::trace::Settled;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};

/// Chassis-owned settlement notification registry. Owned by the
/// chassis (one per substrate); cloned via `Arc` into the
/// [`Mailer`]'s chassis-router closure so the
/// dispatcher's `Settled` switch can fire.
#[derive(Default)]
pub struct SettlementRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Subscribers waiting on each root's settlement signal. Vec so
    /// multiple gate sites can wait on the same root concurrently
    /// (lifecycle gates + the per-frame drain barrier might both
    /// listen on the same Tick root). Channel and mail subscribers
    /// coexist in the same vec, distinguished by [`SettlementSubscriber`]
    /// variant — one map, one drain.
    pending: HashMap<MailId, Vec<SettlementSubscriber>>,
    /// Roots that have already settled at least once. Subscribing to
    /// one pre-fires the receiver. Grows unboundedly within a
    /// chassis lifetime; v1 accepts the bound (chassis tear-down
    /// reclaims).
    settled: HashSet<MailId>,
}

/// One subscriber parked on a root pending settlement. Channel
/// subscribers are for in-thread waiters (chassis-internal code, tests)
/// that block on `Receiver<()>`; mail subscribers are for actors whose
/// thread is committed to its mpsc inbox and can't block on a separate
/// channel without per-cid helper threads.
enum SettlementSubscriber {
    /// Wake an in-thread waiter on a `bounded(1)` channel.
    Channel(Sender<()>),
    /// Push a notification mail to `target` via `mailer` carrying a
    /// [`Settled`] with the settled root as the payload.
    Mail {
        target: MailboxId,
        kind: KindId,
        mailer: Arc<Mailer>,
    },
}

impl SettlementSubscriber {
    /// Fire this subscriber for the settled `root`. Channel sends are
    /// non-blocking (`try_send`, so a closed receiver doesn't panic);
    /// mail sends go through the chassis [`Mailer`]
    /// which resolves the recipient inline on the firing thread.
    fn fire(self, root: MailId) {
        match self {
            Self::Channel(tx) => {
                let _ = tx.try_send(());
            }
            Self::Mail {
                target,
                kind,
                mailer,
            } => {
                push_settlement_notice(&mailer, target, kind, root);
            }
        }
    }
}

impl SettlementRegistry {
    /// Construct an empty registry. Production chassis builders wrap
    /// the result in `Arc<SettlementRegistry>` and clone into both
    /// the chassis context (subscribers reach for it) and the
    /// `Mailer` chassis-router closure (the `Settled` mail dispatch
    /// fires it).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe a gate site to `root`'s settlement signal. Returns
    /// a [`Receiver<()>`] that wakes when [`Self::fire_settled`] is
    /// called for the same root. Pre-fires immediately if `root` has
    /// already settled at least once.
    ///
    /// The receiver carries a single `()` value; subsequent receive
    /// attempts return [`crossbeam_channel::TryRecvError::Empty`] /
    /// `Disconnected` per the bounded(1) channel contract. Gate
    /// sites typically `recv_timeout` once and discard the receiver.
    ///
    /// # Panics
    /// Panics if the inner `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    pub fn subscribe_settlement(&self, root: MailId) -> Receiver<()> {
        let (tx, rx) = bounded::<()>(1);
        let mut inner = self
            .inner
            .lock()
            .expect("settlement registry mutex poisoned; fail-fast per ADR-0063");
        if inner.settled.contains(&root) {
            // Pre-fire — root already settled. `try_send` rather
            // than `send` so a closed receiver (caller dropped it
            // before reading) doesn't panic.
            let _ = tx.try_send(());
        } else {
            inner
                .pending
                .entry(root)
                .or_default()
                .push(SettlementSubscriber::Channel(tx));
        }
        rx
    }

    /// Subscribe a mailbox to receive a notification mail when `root`
    /// settles. The notification is a [`Mail`] with the
    /// given `kind`, a [`Settled`] carrying the settled root as payload,
    /// and `count = 1`. Pre-fires immediately (synchronously
    /// pushes the mail) if `root` has already settled at least once.
    ///
    /// Coexists with [`Self::subscribe_settlement`] — a root can have
    /// channel and mail subscribers; both fire on `fire_settled`.
    ///
    /// # Panics
    /// Panics if the inner `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    pub fn subscribe_settlement_mail(
        &self,
        root: MailId,
        target: MailboxId,
        kind: KindId,
        mailer: Arc<Mailer>,
    ) {
        let mut inner = self
            .inner
            .lock()
            .expect("settlement registry mutex poisoned; fail-fast per ADR-0063");
        if inner.settled.contains(&root) {
            // Drop the mutex before pushing — `push` may run hot
            // (resolves the recipient inline on this thread).
            drop(inner);
            push_settlement_notice(&mailer, target, kind, root);
        } else {
            inner
                .pending
                .entry(root)
                .or_default()
                .push(SettlementSubscriber::Mail {
                    target,
                    kind,
                    mailer,
                });
        }
    }

    /// Fire the settlement signal for `root`. Wakes every subscriber
    /// currently registered for `root` and records the root in the
    /// `settled` set so subsequent [`Self::subscribe_settlement`]
    /// calls pre-fire. Idempotent: calling twice is the same as
    /// calling once for any waiter that already woke.
    ///
    /// # Panics
    /// Panics if the inner `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    pub fn fire_settled(&self, root: MailId) {
        // Drop the mutex before firing — mail subscribers resolve
        // the recipient inline on this thread, and channel sends are
        // cheap but uniformly drop-then-fire keeps the lock window
        // tight and removes a re-entrancy hazard if a future
        // subscriber type re-enters the registry.
        let subs = {
            let mut inner = self
                .inner
                .lock()
                .expect("settlement registry mutex poisoned; fail-fast per ADR-0063");
            inner.settled.insert(root);
            inner.pending.remove(&root)
        };
        if let Some(subs) = subs {
            for sub in subs {
                sub.fire(root);
            }
        }
    }

    /// Test introspection: count of pending channel subscribers
    /// across all roots. Used by the unit tests in this module;
    /// production code queries via mail (subscribe + recv).
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.inner
            .lock()
            .expect("settlement registry mutex poisoned; fail-fast per ADR-0063")
            .pending
            .values()
            .flat_map(|v| v.iter())
            .filter(|s| matches!(s, SettlementSubscriber::Channel(_)))
            .count()
    }

    /// Test introspection: count of roots recorded as already
    /// settled.
    #[cfg(test)]
    fn settled_count(&self) -> usize {
        self.inner
            .lock()
            .expect("settlement registry mutex poisoned; fail-fast per ADR-0063")
            .settled
            .len()
    }

    /// Test introspection: count of pending mail subscribers across all
    /// roots.
    #[cfg(test)]
    fn pending_mail_count(&self) -> usize {
        self.inner
            .lock()
            .expect("settlement registry mutex poisoned; fail-fast per ADR-0063")
            .pending
            .values()
            .flat_map(|v| v.iter())
            .filter(|s| matches!(s, SettlementSubscriber::Mail { .. }))
            .count()
    }
}

/// Push a settlement-notice mail to `target` via `mailer`. The payload
/// is a [`Settled`] carrying the settled root, encoded through the kind
/// codec — the same shape the consumer's `on_settled` handler decodes.
fn push_settlement_notice(mailer: &Mailer, target: MailboxId, kind: KindId, root: MailId) {
    let payload = Settled { root }.encode_into_bytes();
    mailer.push(Mail::new(target, kind, payload, 1));
}

/// What a call site wants done when a wait on an internal completion
/// signal exhausts its cumulative patience budget without the signal
/// firing (issue #1305). The helper owns the *patience strategy*
/// (escalating re-arm + per-round log); the disposition names what a
/// genuine wedge means here so the same "wait for an internal signal"
/// gate doesn't re-roll five divergent terminal behaviors by hand.
///
/// The variants line up with the five behaviors already scattered
/// across the substrate + its bundle. The helper dispenses `Proceed`,
/// `ReplyErr`, and `Panic` directly via its return value / a `panic!`;
/// `Abort` is the one disposition that needs an aborter (a
/// [`crate::runtime::lifecycle::FatalAborter`]), which the caller holds
/// — the helper stays free of any `HubOutbound` coupling and hands the
/// caller a [`GateWedge`] to route through its own aborter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDisposition {
    /// Best-effort gate: log loud and let the caller carry on. Never
    /// blocks process exit. The helper returns
    /// [`WaitOutcome::Wedged`] so the caller can branch, but takes no
    /// terminal action itself.
    Proceed,
    /// The wedge is recoverable from the caller's vantage — it surfaces
    /// a typed error to someone who can retry. The helper returns
    /// [`WaitOutcome::Wedged`]; the caller maps it to its error type.
    ReplyErr,
    /// The wedge is unrecoverable: the caller must route the returned
    /// [`GateWedge`] through its [`crate::runtime::lifecycle::FatalAborter`].
    /// The helper does *not* call `fatal_abort` itself — that would
    /// couple this module to the desktop chassis's `HubOutbound`.
    Abort,
    /// Test/debug attributable failure: the helper `panic!`s on a wedge
    /// so a stuck gate fails at the gate site rather than as a
    /// downstream `0 != 1`. (A `panic!` needs no aborter, so the helper
    /// can dispense this one without coupling to the outbound.)
    Panic,
}

/// A wedge detected by [`await_internal_signal`]: the internal signal
/// did not fire within the cumulative patience budget. Carries enough
/// context for the caller to log / surface / abort attributably.
#[derive(Debug, Clone)]
pub struct GateWedge {
    /// The gate name passed to [`await_internal_signal`].
    pub gate: String,
    /// Total wall-clock time waited before giving up.
    pub waited: Duration,
    /// Whether the channel disconnected (sender dropped without firing)
    /// rather than simply staying silent to the cap. Both are wedges;
    /// the distinction is diagnostic.
    pub disconnected: bool,
}

impl GateWedge {
    /// Render the wedge as the `reason` string a
    /// [`crate::runtime::lifecycle::FatalAborter`] consumes.
    #[must_use]
    pub fn reason(&self) -> String {
        let cause = if self.disconnected {
            "signal channel disconnected without firing"
        } else {
            "internal signal never fired within patience budget"
        };
        format!(
            "gate {} wedged: {cause}, waited {:?}",
            self.gate, self.waited
        )
    }
}

/// Outcome of an [`await_internal_signal`] wait.
#[derive(Debug)]
#[must_use]
pub enum WaitOutcome {
    /// The internal signal fired before the cumulative cap. Proceed
    /// normally.
    Settled,
    /// The signal never fired (silent to the cap, or the channel
    /// disconnected). The caller dispenses its [`TerminalDisposition`]
    /// against the carried [`GateWedge`]. Only returned for the
    /// non-`Panic` dispositions — `Panic` diverges inside the helper.
    Wedged(GateWedge),
}

/// Escalating-patience wait on an internal completion signal — a
/// settlement [`Receiver`] or a pooled-actor teardown close-done
/// channel (issue #1305). Replaces the hand-rolled wall-clock
/// `recv_timeout(N)` that can't tell *starved-but-healthy* (the causal
/// chain is merely slow under load) from *genuinely wedged*: under
/// `nextest --workspace` saturation a healthy-but-slow gate trips a
/// fixed deadline and false-fires (flake #1295).
///
/// Loops `rx.recv_timeout(round_budget)`:
///
/// - `Ok(())` → [`WaitOutcome::Settled`].
/// - `Timeout` → log `gate <name> slow: waited <cumulative>, extending`
///   at warn and re-arm, until the cumulative wait reaches
///   `cumulative_cap`. The signal is a one-shot the worker fires
///   whenever it is next scheduled, so re-arming `recv` is patient
///   waiting with logged checkpoints, not a re-poke — a healthy gate
///   resolves before the cap; a genuine wedge exhausts it.
/// - `Disconnected` → the sender dropped without firing; the same
///   terminal path as cap-exhaustion, with [`GateWedge::disconnected`]
///   set so the wedge is attributable.
///
/// On a wedge the helper dispenses `disposition`:
///
/// - [`TerminalDisposition::Panic`] → diverges via `panic!` (no aborter
///   needed; keeps the wedge attributable in test/debug).
/// - [`TerminalDisposition::Proceed`] / [`TerminalDisposition::ReplyErr`]
///   / [`TerminalDisposition::Abort`] → returns [`WaitOutcome::Wedged`]
///   carrying the [`GateWedge`]; the caller acts on it (log-and-carry,
///   typed error, or route through its `FatalAborter`). `Abort` is
///   *not* performed here — that would couple this module to the
///   desktop chassis's `HubOutbound`.
///
/// `round_budget` is one re-arm interval (the log cadence);
/// `cumulative_cap` is the total patience before declaring a wedge. A
/// `round_budget` of zero is clamped up to a small floor so the loop
/// can't spin.
pub fn await_internal_signal(
    rx: &Receiver<()>,
    gate: &str,
    round_budget: Duration,
    cumulative_cap: Duration,
    disposition: TerminalDisposition,
) -> WaitOutcome {
    // Clamp the per-round budget off zero so a misconfigured caller
    // can't turn the loop into a busy-spin.
    let round = round_budget.max(Duration::from_millis(1));
    let start = Instant::now();
    loop {
        match rx.recv_timeout(round) {
            Ok(()) => return WaitOutcome::Settled,
            Err(RecvTimeoutError::Timeout) => {
                let waited = start.elapsed();
                if waited >= cumulative_cap {
                    return wedge(gate, waited, false, disposition);
                }
                tracing::warn!(
                    target: "aether_substrate::settlement",
                    gate,
                    waited_millis = waited.as_millis(),
                    cap_millis = cumulative_cap.as_millis(),
                    "gate {gate} slow: waited {waited:?}, extending",
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                return wedge(gate, start.elapsed(), true, disposition);
            }
        }
    }
}

/// Build the wedge verdict and dispense the one disposition the helper
/// owns (`Panic`); the rest ride back to the caller in
/// [`WaitOutcome::Wedged`].
fn wedge(
    gate: &str,
    waited: Duration,
    disconnected: bool,
    disposition: TerminalDisposition,
) -> WaitOutcome {
    let wedge = GateWedge {
        gate: gate.to_owned(),
        waited,
        disconnected,
    };
    match disposition {
        TerminalDisposition::Panic => panic!("{}", wedge.reason()),
        TerminalDisposition::Proceed
        | TerminalDisposition::ReplyErr
        | TerminalDisposition::Abort => WaitOutcome::Wedged(wedge),
    }
}

#[cfg(test)]
// Settlement tests hold per-test `Mutex` guards across the assertion
// sequence so the captured state stays consistent against the
// concurrent firing thread.
#[allow(clippy::significant_drop_tightening)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
mod tests {
    use super::*;
    use crate::handle_store::HandleStore;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::OwnedDispatch;
    use crate::mail::registry::Registry;
    use std::sync::Mutex as StdMutex;
    use std::thread;

    fn root(sender: u64, cid: u64) -> MailId {
        MailId {
            sender: MailboxId(sender),
            correlation_id: cid,
        }
    }

    /// One captured dispatch — what the test asserts against.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct CapturedDispatch {
        kind: KindId,
        payload: Vec<u8>,
        count: u32,
    }

    /// Build a fresh `Mailer` backed by a registry + handle store
    /// pair. Registers a closure-bound sink under `sink_name` that
    /// captures the dispatched mails into a shared buffer the test
    /// asserts against. Returns the mailer, the registered sink's
    /// mailbox id, and the buffer.
    fn fresh_mailer_with_sink(
        sink_name: &str,
    ) -> (Arc<Mailer>, MailboxId, Arc<StdMutex<Vec<CapturedDispatch>>>) {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let captured: Arc<StdMutex<Vec<CapturedDispatch>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let target = registry.register_inbox(
            sink_name,
            // iamacoffeepot/aether#848 PR 3: take `OwnedDispatch`
            // directly and move payload into the captured row
            // (was: `payload.to_vec()` clone via the legacy
            // borrowed-dispatch shape).
            Arc::new(move |dispatch: OwnedDispatch| {
                // ADR-0094: terminal test consumer — discharge before the
                // partial-move of `payload` below.
                dispatch.discharge();
                captured_clone.lock().unwrap().push(CapturedDispatch {
                    kind: dispatch.kind,
                    payload: dispatch.payload.into_vec(),
                    count: dispatch.count,
                });
            }),
        );
        (mailer, target, captured)
    }

    #[test]
    fn subscribe_then_fire_wakes_receiver() {
        let reg = SettlementRegistry::new();
        let r = root(1, 1);
        let rx = reg.subscribe_settlement(r);
        assert_eq!(reg.pending_count(), 1);
        reg.fire_settled(r);
        assert_eq!(reg.pending_count(), 0);
        assert_eq!(reg.settled_count(), 1);
        rx.recv().expect("settlement signal");
    }

    #[test]
    fn fire_then_subscribe_pre_fires_receiver() {
        let reg = SettlementRegistry::new();
        let r = root(1, 1);
        reg.fire_settled(r);
        assert_eq!(reg.settled_count(), 1);
        let rx = reg.subscribe_settlement(r);
        // Subscriber landed in the settled-set fast path — no
        // pending entry was added.
        assert_eq!(reg.pending_count(), 0);
        rx.recv().expect("pre-fired signal");
    }

    #[test]
    fn multiple_subscribers_all_wake() {
        let reg = SettlementRegistry::new();
        let r = root(1, 1);
        let rx1 = reg.subscribe_settlement(r);
        let rx2 = reg.subscribe_settlement(r);
        let rx3 = reg.subscribe_settlement(r);
        assert_eq!(reg.pending_count(), 3);
        reg.fire_settled(r);
        rx1.recv().expect("subscriber 1 wakes");
        rx2.recv().expect("subscriber 2 wakes");
        rx3.recv().expect("subscriber 3 wakes");
    }

    #[test]
    fn fire_twice_is_idempotent() {
        let reg = SettlementRegistry::new();
        let r = root(1, 1);
        let rx = reg.subscribe_settlement(r);
        reg.fire_settled(r);
        reg.fire_settled(r);
        // First fire wakes the subscriber; second is a no-op for the
        // already-drained pending entry.
        rx.recv().expect("first fire wakes");
        assert_eq!(reg.settled_count(), 1);
    }

    #[test]
    fn distinct_roots_are_independent() {
        let reg = SettlementRegistry::new();
        let r1 = root(1, 1);
        let r2 = root(1, 2);
        let rx1 = reg.subscribe_settlement(r1);
        let rx2 = reg.subscribe_settlement(r2);
        reg.fire_settled(r1);
        rx1.recv().expect("r1 wakes");
        // r2's subscriber stays parked.
        assert!(rx2.try_recv().is_err());
        reg.fire_settled(r2);
        rx2.recv().expect("r2 wakes");
    }

    /// `subscribe_settlement_mail` then `fire_settled`: one mail is
    /// pushed to the subscribed target with the expected `(kind,
    /// payload-decodes-to-root)`.
    #[test]
    fn subscribe_mail_then_fire_pushes_notification() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.subscribe_fire");
        let r = root(1, 1);
        let kind = KindId(0xABCD);

        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        assert_eq!(reg.pending_mail_count(), 1);
        reg.fire_settled(r);
        assert_eq!(reg.pending_mail_count(), 0);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let mail = &captured[0];
        assert_eq!(mail.kind, kind);
        assert_eq!(mail.count, 1);
        let decoded = Settled::decode_from_bytes(&mail.payload)
            .expect("decode Settled")
            .root;
        assert_eq!(decoded, r);
    }

    /// `fire_settled` first, then `subscribe_settlement_mail`: the
    /// notification pre-fires synchronously.
    #[test]
    fn fire_then_subscribe_mail_pre_fires() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.fire_subscribe");
        let r = root(2, 4);
        let kind = KindId(0x1234);

        reg.fire_settled(r);
        assert!(captured.lock().unwrap().is_empty());

        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        // Pre-fire path: no parked entry should remain.
        assert_eq!(reg.pending_mail_count(), 0);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].kind, kind);
        let decoded = Settled::decode_from_bytes(&captured[0].payload)
            .expect("decode Settled")
            .root;
        assert_eq!(decoded, r);
    }

    /// Three mail subscribers on the same root all receive a
    /// notification when `fire_settled` runs.
    #[test]
    fn multiple_mail_subscribers_all_receive() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.multi");
        let r = root(3, 9);
        let kind = KindId(0x5555);

        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        assert_eq!(reg.pending_mail_count(), 3);

        reg.fire_settled(r);
        assert_eq!(reg.pending_mail_count(), 0);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 3);
        for entry in captured.iter() {
            assert_eq!(entry.kind, kind);
            let decoded = Settled::decode_from_bytes(&entry.payload)
                .expect("decode Settled")
                .root;
            assert_eq!(decoded, r);
        }
    }

    /// A channel subscriber and a mail subscriber on the same root
    /// both fire when `fire_settled` runs.
    #[test]
    fn channel_and_mail_subscribers_coexist() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.coexist");
        let r = root(4, 16);
        let kind = KindId(0x7777);

        let rx = reg.subscribe_settlement(r);
        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        assert_eq!(reg.pending_count(), 1);
        assert_eq!(reg.pending_mail_count(), 1);

        reg.fire_settled(r);
        assert_eq!(reg.pending_count(), 0);
        assert_eq!(reg.pending_mail_count(), 0);

        rx.recv().expect("channel subscriber wakes");
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].kind, kind);
    }

    /// Mail subscribers on distinct roots fire independently — settling
    /// r1 does not fire r2's mail subscription.
    #[test]
    fn distinct_roots_independent_for_mail() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.distinct");
        let r1 = root(5, 25);
        let r2 = root(5, 36);
        let kind = KindId(0x9999);

        reg.subscribe_settlement_mail(r1, target, kind, Arc::clone(&mailer));
        reg.subscribe_settlement_mail(r2, target, kind, Arc::clone(&mailer));
        assert_eq!(reg.pending_mail_count(), 2);

        reg.fire_settled(r1);
        assert_eq!(reg.pending_mail_count(), 1);

        let after_r1 = captured.lock().unwrap().clone();
        assert_eq!(after_r1.len(), 1);
        let decoded = Settled::decode_from_bytes(&after_r1[0].payload)
            .expect("decode Settled")
            .root;
        assert_eq!(decoded, r1);

        reg.fire_settled(r2);
        assert_eq!(reg.pending_mail_count(), 0);

        let after_r2 = captured.lock().unwrap().clone();
        assert_eq!(after_r2.len(), 2);
        let decoded = Settled::decode_from_bytes(&after_r2[1].payload)
            .expect("decode Settled")
            .root;
        assert_eq!(decoded, r2);
    }

    /// Resolves-within-cap: the signal fires after one round elapses
    /// (exercising the re-arm path), and the helper returns `Settled`.
    #[test]
    fn await_internal_signal_resolves_after_rearm() {
        let (tx, rx) = bounded::<()>(1);
        // Fire from a sibling thread after roughly one round budget so
        // the first `recv_timeout` times out (logging + re-arm) and the
        // second resolves.
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let _ = tx.try_send(());
        });
        let outcome = await_internal_signal(
            &rx,
            "test.rearm",
            Duration::from_millis(10),
            Duration::from_secs(5),
            TerminalDisposition::Proceed,
        );
        handle.join().expect("firing thread joins");
        assert!(matches!(outcome, WaitOutcome::Settled));
    }

    /// Cap-exhaustion: the signal never fires, so the helper exhausts
    /// the cumulative cap and returns `Wedged` (not disconnected) for a
    /// non-`Panic` disposition.
    #[test]
    fn await_internal_signal_cap_exhaustion_wedges() {
        // Hold the sender alive so the channel doesn't disconnect —
        // this is the silent-to-cap path, distinct from `Disconnected`.
        let (_tx, rx) = bounded::<()>(1);
        let outcome = await_internal_signal(
            &rx,
            "test.cap",
            Duration::from_millis(5),
            Duration::from_millis(20),
            TerminalDisposition::ReplyErr,
        );
        match outcome {
            WaitOutcome::Wedged(w) => {
                assert!(!w.disconnected);
                assert_eq!(w.gate, "test.cap");
                assert!(w.waited >= Duration::from_millis(20));
            }
            WaitOutcome::Settled => panic!("expected a wedge, got Settled"),
        }
    }

    /// `Disconnected`: dropping the sender takes the same terminal path
    /// as cap-exhaustion, with `disconnected` set.
    #[test]
    fn await_internal_signal_disconnect_wedges() {
        let (tx, rx) = bounded::<()>(1);
        drop(tx);
        let outcome = await_internal_signal(
            &rx,
            "test.disconnect",
            Duration::from_millis(50),
            Duration::from_secs(5),
            TerminalDisposition::Proceed,
        );
        match outcome {
            WaitOutcome::Wedged(w) => {
                assert!(w.disconnected);
                assert_eq!(w.gate, "test.disconnect");
            }
            WaitOutcome::Settled => panic!("expected a wedge, got Settled"),
        }
    }

    /// `Panic` disposition diverges inside the helper on a wedge —
    /// asserts the gate fails attributably at the gate site.
    #[test]
    #[should_panic(expected = "gate test.panic wedged")]
    fn await_internal_signal_panic_disposition_diverges() {
        let (tx, rx) = bounded::<()>(1);
        drop(tx);
        let _ = await_internal_signal(
            &rx,
            "test.panic",
            Duration::from_millis(5),
            Duration::from_millis(20),
            TerminalDisposition::Panic,
        );
    }

    /// The settlement-notice payload decodes back to the
    /// subscribed root — direct check of the wire contract.
    #[test]
    fn mail_payload_decodes_to_root() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.payload");
        let r = root(7, 49);
        let kind = KindId(0x4321);

        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        reg.fire_settled(r);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let decoded = Settled::decode_from_bytes(&captured[0].payload)
            .expect("decode Settled")
            .root;
        assert_eq!(decoded, r);
    }
}
