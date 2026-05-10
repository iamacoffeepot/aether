//! ADR-0080 §6 settlement registry — chassis-side gate-notification
//! map for `Settled { root }` mail.
//!
//! Subscribers (lifecycle gates, the per-frame Tick drain, the
//! `replace_component` drain — landing in PR 4) call
//! [`SettlementRegistry::subscribe_settlement`] with the root
//! `MailId` of the causal chain they want to wait on. They get a
//! `crossbeam_channel::Receiver<()>` that fires when the
//! [`crate::actor::native`] dispatcher routes a `Settled { root }`
//! mail addressed to [`aether_data::MailboxId::CHASSIS_MAILBOX_ID`]
//! through the registry's [`SettlementRegistry::fire_settled`] hook.
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
//! The `settled` HashSet grows unboundedly within a chassis lifetime
//! today. PR 5 (or a later cleanup) wires retention against the
//! observer's eviction policy. For v1 — a chassis runs for a session,
//! not forever — the cap-by-count plus per-process tear-down keeps
//! memory bounded.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use aether_data::MailId;
use crossbeam_channel::{Receiver, Sender, bounded};

/// Chassis-owned settlement notification registry. Owned by the
/// chassis (one per substrate); cloned via `Arc` into the
/// [`crate::mail::Mailer`]'s chassis-router closure so the
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
    /// listen on the same Tick root).
    pending: HashMap<MailId, Vec<Sender<()>>>,
    /// Roots that have already settled at least once. Subscribing to
    /// one pre-fires the receiver. Grows unboundedly within a
    /// chassis lifetime; v1 accepts the bound (chassis tear-down
    /// reclaims).
    settled: HashSet<MailId>,
}

impl SettlementRegistry {
    /// Construct an empty registry. Production chassis builders wrap
    /// the result in `Arc<SettlementRegistry>` and clone into both
    /// the chassis context (subscribers reach for it) and the
    /// `Mailer` chassis-router closure (the `Settled` mail dispatch
    /// fires it).
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
    pub fn subscribe_settlement(&self, root: MailId) -> Receiver<()> {
        let (tx, rx) = bounded::<()>(1);
        let mut inner = self.inner.lock().unwrap();
        if inner.settled.contains(&root) {
            // Pre-fire — root already settled. `try_send` rather
            // than `send` so a closed receiver (caller dropped it
            // before reading) doesn't panic.
            let _ = tx.try_send(());
        } else {
            inner.pending.entry(root).or_default().push(tx);
        }
        rx
    }

    /// Fire the settlement signal for `root`. Wakes every subscriber
    /// currently registered for `root` and records the root in the
    /// `settled` set so subsequent [`Self::subscribe_settlement`]
    /// calls pre-fire. Idempotent: calling twice is the same as
    /// calling once for any waiter that already woke.
    pub fn fire_settled(&self, root: MailId) {
        let mut inner = self.inner.lock().unwrap();
        inner.settled.insert(root);
        if let Some(subs) = inner.pending.remove(&root) {
            for tx in subs {
                let _ = tx.try_send(());
            }
        }
    }

    /// Test introspection: count of pending subscribers across all
    /// roots. Used by the unit tests in this module; production code
    /// queries via mail (subscribe + recv).
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .pending
            .values()
            .map(Vec::len)
            .sum()
    }

    /// Test introspection: count of roots recorded as already
    /// settled.
    #[cfg(test)]
    fn settled_count(&self) -> usize {
        self.inner.lock().unwrap().settled.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(sender: u64, cid: u64) -> MailId {
        MailId {
            sender: aether_data::MailboxId(sender),
            correlation_id: cid,
        }
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
}
