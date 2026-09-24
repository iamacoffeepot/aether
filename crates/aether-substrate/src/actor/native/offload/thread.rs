//! ADR-0080 §12 thread-spawn primitives.
//!
//! A native actor that has work too slow for its handler turn moves it to a
//! worker thread through one of the two spawns here. The worker is a plain
//! closure: it receives no ctx and sends no mail. The two shapes differ only
//! in the settlement hold:
//!
//! - [`NativeCtx::spawn_inherit`](crate::actor::native::NativeCtx::spawn_inherit)
//!   holds the spawning handler's chain open for the worker's life, so the
//!   chain does not settle until the worker exits.
//! - [`NativeCtx::spawn_detached`](crate::actor::native::NativeCtx::spawn_detached)
//!   holds nothing. The worker answers to no chain.
//!
//! A thread that has to reach its actor again wakes it through
//! [`SelfWake`](crate::actor::native::offload::self_wake::SelfWake); work that
//! replies in a later handler turn uses ADR-0093's `dispatch_blocking`.
//!
//! ## Settlement contract (ADR-0080 §12, iamacoffeepot/aether#716)
//!
//! ADR-0080 §12 says the spawning handler's tree does not settle until its
//! spawned work completes. Enforced here via the
//! [`SettlementHold`](crate::runtime::trace::SettlementHold) RAII guard from
//! [`crate::Mailer::acquire_settlement_hold`]: `spawn_inherit` acquires a
//! hold against the parent's `in_flight_root` BEFORE the worker thread is
//! spawned (so the `held_open` increment lands ahead of the parent handler's
//! `Finished`), then moves the hold into the closure the worker runs. Drop
//! fires `Release`. Settlement gates on `(in_flight == 0 && held_open == 0)`,
//! so a worker thread that outlives its handler keeps the chain open until it
//! exits.
//!
//! A panic in either worker is fatal (ADR-0063): the body runs under
//! `fail_fast::run_or_abort` with the aborter taken before the spawn, so the
//! escalation holds whatever state the owning actor is in. The hold lives
//! inside that body, so an unwinding `spawn_inherit` worker drops it before
//! the abort.

use std::thread::{self, JoinHandle};

use aether_data::MailId;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::offload::fail_fast;

/// ADR-0080 §12 thread-spawn helper. Spawns a thread named for `namespace`
/// that runs `f` while holding the spawning handler's `in_flight_root` open.
/// The spawning function is the `spawn_inherit` entry point on
/// [`crate::actor::native::ctx::NativeCtx`]; this function is the crate-private
/// runtime body it delegates to.
// This IS the spawn_inherit primitive (ADR-0080 §12) — the sanctioned raw spawn
// the lint points callers at; it cannot route through itself.
#[allow(clippy::disallowed_methods)]
pub(crate) fn spawn_inherit(
    binding: &NativeBinding,
    in_flight_root: Option<MailId>,
    namespace: &'static str,
    f: impl FnOnce() + Send + 'static,
) -> JoinHandle<()> {
    // ADR-0080 §12 / iamacoffeepot/aether#716: acquire the settlement
    // hold on the parent thread BEFORE spawning, so by the time the
    // parent handler's `Finished` drops `in_flight` to zero the
    // `held_open` counter is already non-zero. The hold moves into the
    // body `run_or_abort` runs, so release fires on worker exit — and on
    // an unwinding worker before the abort escalates.
    //
    // A handler without an in-flight root has no chain to keep open, so
    // the acquire hands back no hold.
    let hold = in_flight_root.map(|root| binding.mailer().acquire_settlement_hold(root));
    let aborter = binding.fatal_aborter();
    thread::Builder::new()
        .name(format!("aether-inherit-{namespace}"))
        .spawn(move || {
            fail_fast::run_or_abort(aborter.as_ref(), "spawn_inherit worker", move || {
                let _hold = hold;
                f();
            });
        })
        .expect("spawn aether-inherit thread")
}

/// ADR-0080 §12 thread-spawn helper. Spawns a thread named for `namespace`
/// that runs `f` holding no chain.
// This IS the spawn_detached primitive (ADR-0080 §12) — the sanctioned raw spawn
// the lint points callers at; it cannot route through itself.
#[allow(clippy::disallowed_methods)]
pub(crate) fn spawn_detached(
    binding: &NativeBinding,
    namespace: &'static str,
    f: impl FnOnce() + Send + 'static,
) -> JoinHandle<()> {
    let aborter = binding.fatal_aborter();
    thread::Builder::new()
        .name(format!("aether-root-{namespace}"))
        .spawn(move || fail_fast::run_or_abort(aborter.as_ref(), "spawn_detached worker", f))
        .expect("spawn aether-root thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, OnceLock};

    use aether_data::MailboxId;

    use crate::chassis::settlement_table::SettlementTable;
    use crate::mail::Mailer;
    use crate::mail::registry::Registry;
    use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
    use crate::runtime::panic_hook::payload_string;

    const NAMESPACE: &str = "test.spawn_thread.stub";

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        (registry, mailer)
    }

    /// Records `root`'s `held_open` count at the moment a worker escalates,
    /// then aborts as [`PanicAborter`] does.
    struct HoldProbeAborter {
        counter: Arc<SettlementTable>,
        root: MailId,
        held_at_abort: OnceLock<u32>,
    }

    impl FatalAborter for HoldProbeAborter {
        fn abort(&self, reason: String) -> ! {
            self.held_at_abort.set(self.counter.held_open(self.root)).expect("the worker aborts once");
            PanicAborter.abort(reason)
        }
    }

    /// ADR-0080 §12 / iamacoffeepot/aether#716: `spawn_inherit`
    /// acquires a `SettlementHold` on the inherited root before spawning
    /// and drops it on thread exit. Post-ADR-0086 Phase 3c holds are
    /// counter-only (no trace-queue events), so we observe the hold
    /// through the emit-time `SettlementCounter`: the worker blocks on a
    /// gate so the hold is observably open before we release it.
    #[test]
    fn spawn_inherit_acquires_and_releases_settlement_hold() {
        use std::sync::mpsc::channel;

        let (_registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let producer_mailbox = MailboxId(0xC0FE_C0FE_C0FE_C0FE);
        let binding = NativeBinding::new_for_test(Arc::clone(&mailer), producer_mailbox);
        let inherited_root = MailId::new(MailboxId(0xC0FE_C0FE_C0FE_C0FE), 9001);

        // Gate the worker so the hold stays open across the assertion.
        let (gate_tx, gate_rx) = channel::<()>();
        let join = spawn_inherit(&binding, Some(inherited_root), NAMESPACE, move || {
            // Block until released — the SettlementHold (moved into
            // this worker's body) is held for the whole body.
            let _ = gate_rx.recv();
        });

        // The hold is acquired on the parent thread before the spawn, so
        // it is open now regardless of worker scheduling.
        assert_eq!(
            counter.held_open(inherited_root),
            1,
            "spawn_inherit must acquire a settlement hold on the inherited root"
        );

        gate_tx.send(()).expect("release worker");
        join.join().expect("inherit worker thread joins");

        // The worker body dropped the hold on exit → hold released → the
        // (0, 0) cell is reclaimed.
        assert_eq!(counter.held_open(inherited_root), 0, "the hold must release when the worker exits");
    }

    /// A panic in a `spawn_inherit` worker escalates through the binding's
    /// chassis aborter with the payload in the reason (ADR-0063), and the
    /// unwinding worker drops its settlement hold before the abort. The
    /// probe aborter reads the hold count when it fires, then panics as
    /// `PanicAborter` does, so the joined payload is the aborter's own
    /// message; a worker run bare joins with the probe alone.
    #[test]
    fn spawn_inherit_worker_panic_escalates_through_the_aborter() {
        let (_registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let inherited_root = MailId::new(MailboxId(0x6431), 1);
        let aborter = Arc::new(HoldProbeAborter {
            counter: Arc::clone(&counter),
            root: inherited_root,
            held_at_abort: OnceLock::new(),
        });
        let binding = NativeBinding::new(
            Arc::clone(&mailer),
            MailboxId(0x6431),
            0x6431,
            Arc::from(NAMESPACE),
            Arc::<HoldProbeAborter>::clone(&aborter),
            None,
        );

        let payload = spawn_inherit(&binding, Some(inherited_root), NAMESPACE, || {
            panic!("inherit probe 6431");
        })
        .join()
        .expect_err("the worker panics");
        let reason = payload_string(payload.as_ref());

        assert!(reason.contains("fatal abort"), "the aborter ran: {reason}");
        assert!(
            reason.contains("spawn_inherit worker panicked: inherit probe 6431"),
            "reason carries the payload: {reason}"
        );
        assert_eq!(aborter.held_at_abort.get(), Some(&0), "the unwinding worker released its hold before the abort");
        assert_eq!(counter.held_open(inherited_root), 0, "the unwinding worker released its hold");
    }

    /// A panic in a `spawn_detached` worker escalates through the binding's
    /// chassis aborter with the payload in the reason (ADR-0063).
    #[test]
    fn spawn_detached_worker_panic_escalates_through_the_aborter() {
        let (_registry, mailer) = fresh_substrate();
        let binding = NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x6432));

        let payload = spawn_detached(&binding, NAMESPACE, || {
            panic!("detached probe 6431");
        })
        .join()
        .expect_err("the worker panics");
        let reason = payload_string(payload.as_ref());

        assert!(reason.contains("fatal abort"), "the aborter ran: {reason}");
        assert!(
            reason.contains("spawn_detached worker panicked: detached probe 6431"),
            "reason carries the payload: {reason}"
        );
    }

    /// An absent inherited root skips the hold — there's no chain to keep
    /// open. Verify a rootless spawn creates no settlement cell.
    #[test]
    fn spawn_inherit_without_root_skips_hold() {
        let (_registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let producer_mailbox = MailboxId(0xC0FE_DEAD_C0FE_DEAD);
        let binding = NativeBinding::new_for_test(Arc::clone(&mailer), producer_mailbox);

        let live_before = counter.live_roots();
        let join = spawn_inherit(&binding, None, NAMESPACE, || {});
        join.join().expect("inherit worker thread joins");

        assert_eq!(counter.live_roots(), live_before, "a rootless spawn must not create a settlement cell");
    }
}
