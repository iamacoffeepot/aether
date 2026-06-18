//! ADR-0080 substrate-wide mail tracing — chassis-side runtime,
//! slimmed by ADR-0086 Phase 3c.
//!
//! The producer-side helpers ([`TraceHandle::record_sent`] /
//! [`TraceHandle::record_finished`], plus
//! [`TraceHandle::acquire_settlement_hold`]) do two independent jobs:
//!
//! - **Trace** — `record_sent` pushes a [`TraceEvent::Sent`] into the
//!   producing actor's per-actor [`ActorTraceRing`] (via
//!   [`TraceHandle::push_trace_ring`]); the dispatch loop pushes the
//!   matching `Received` / `Finished` into the recipient actor's ring.
//!   Mail produced outside any actor's dispatch (chassis-root / injected
//!   mail — `Tick`, MCP sends, test injects) falls back to the
//!   chassis-host ring on this handle. A trace tree is reconstructed by
//!   walking the rings (`aether.trace.tail`) and stitching client-side
//!   (ADR-0086 Phase 3b) — there is no central queue, drainer, or
//!   observer fold (those retired in Phase 3c).
//! - **Settlement** — every hook funnels its root into the emit-time
//!   [`SettlementTable`], and on the `(in_flight, held_open)`
//!   zero-transition fires `Settled` synchronously through the chassis
//!   [`SettlementRegistry`], so the lifecycle gate wakes the instant
//!   work finishes (ADR-0086 Phase 2). Settlement is independent of the
//!   trace rings.
//!
//! Calls are no-ops when the chassis hasn't installed a handle — silent
//! drop is the right shape for tests that don't bring up the chassis.
//! The handle is owned by the chassis `Mailer` (one per chassis) and
//! reached via `Mailer::trace_handle`.

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

use aether_data::{KindId, MailId, MailboxId};
use aether_kinds::trace::{Nanos, TraceEvent, TraceTail, TraceTailResult};

use aether_actor::Local;
use aether_actor::trace_ring::ActorTraceRing;

use crate::chassis::settlement::SettlementRegistry;
use crate::chassis::settlement_table::SettlementTable;

/// Per-chassis trace-pipeline handle. Owned by the chassis `Mailer`;
/// producer-side hooks reach it via `mailer.trace_handle()` or the
/// `mailer.record_*` shortcuts that wrap the methods on this type.
///
/// Cloning shares the emit-time [`SettlementTable`] + the chassis-host
/// ring (both `Arc`) and copies the boot-time anchor (`Copy`), so every
/// producer site can hold an independent `TraceHandle` cheaply.
///
/// The counter is the settlement authority (ADR-0086 Phase 2): every
/// producer hook funnels its root into it, and on the
/// `(in_flight, held_open)` zero-transition fires `Settled`
/// synchronously through the chassis [`SettlementRegistry`] — so the
/// lifecycle gate wakes the instant work finishes. The registry is
/// installed at boot via [`Self::install_settlement_registry`]; before
/// install (boot is quiescent, so no real traffic) the zero-transition
/// simply skips the fire.
#[derive(Clone)]
pub struct TraceHandle {
    boot_time: Instant,
    settlement_counter: Arc<SettlementTable>,
    settlement_registry: Arc<OnceLock<Arc<SettlementRegistry>>>,
    /// ADR-0086 Phase 3: ring for trace events produced *outside* any
    /// actor's dispatch — chassis-root / injected mail (`Tick`, MCP
    /// sends, test injects) records its `Sent` off any actor's stamped
    /// `ActorSlots`. A `Mutex` (not the `Local` per-actor rings' lock-
    /// free path) because off-actor producers run on arbitrary threads.
    /// The guided walk reads this ring via [`Self::chassis_host_tail`].
    chassis_host_ring: Arc<Mutex<ActorTraceRing>>,
}

// Manual `Debug` — `SettlementRegistry` carries non-`Debug` subscriber
// handles (a `Mailer`), so the registry is summarised as an
// installed/absent flag rather than printed.
impl fmt::Debug for TraceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceHandle")
            .field("boot_time", &self.boot_time)
            .field(
                "settlement_registry_installed",
                &self.settlement_registry.get().is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl TraceHandle {
    /// Build a fresh handle: zeroed settlement counter, empty
    /// chassis-host ring, `Instant::now()` boot anchor. The chassis
    /// builder calls this once per `build_passive`; the returned handle
    /// is installed on the chassis `Mailer`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            boot_time: Instant::now(),
            settlement_counter: Arc::new(SettlementTable::new()),
            settlement_registry: Arc::new(OnceLock::new()),
            chassis_host_ring: Arc::new(Mutex::new(ActorTraceRing::default())),
        }
    }

    /// Issue 1990: set the chassis-host ring's capacity to `ring_cap`.
    /// Called once at chassis boot from `boot_passives` with the resolved
    /// trace-ring capacity so off-actor trace producers (chassis-root /
    /// injected mail) lap at the same configured depth as the per-actor
    /// rings. The ring is empty at boot, so replacing it loses nothing.
    ///
    /// # Panics
    /// Panics if the chassis-host ring mutex is poisoned (fail-fast per
    /// ADR-0063) — unreachable at boot before any producer runs.
    pub fn set_chassis_host_ring_capacity(&self, ring_cap: usize) {
        *self
            .chassis_host_ring
            .lock()
            .expect("chassis-host trace ring mutex poisoned; fail-fast per ADR-0063") =
            ActorTraceRing::with_capacity(ring_cap);
    }

    /// Install the chassis [`SettlementRegistry`] so the emit-time counter
    /// can fire `Settled` on the zero-transition (ADR-0086 Phase 2).
    /// Called once at chassis boot, after the registry is constructed; a
    /// second install is a no-op (the `OnceLock` keeps the first).
    pub fn install_settlement_registry(&self, registry: Arc<SettlementRegistry>) {
        let _ = self.settlement_registry.set(registry);
    }

    /// The emit-time settlement authority — the lock-free
    /// [`SettlementTable`] (ADR-0086 Phase 2; iamacoffeepot/aether#1059
    /// swapped the striped-lock map for the open-addressing table).
    /// Exposed for diagnostics / tests (`live_roots`); production
    /// settlement flows through the producer hooks on this handle.
    #[must_use]
    pub fn settlement_counter(&self) -> &Arc<SettlementTable> {
        &self.settlement_counter
    }

    /// Fire `Settled` for `root` through the installed registry, if one
    /// is installed. Called on the emit-time zero-transition from the
    /// producing thread.
    #[inline]
    fn fire_settled(&self, root: MailId) {
        if let Some(registry) = self.settlement_registry.get() {
            registry.fire_settled(root);
        }
    }

    /// ADR-0086 Phase 3: push a trace event into the per-actor
    /// [`ActorTraceRing`]. Lands in the current actor's ring when one is
    /// stamped — `Sent` on the sender's dispatch (this hook runs inside
    /// the sender's handler), `Received` / `Finished` on the
    /// recipient's (the dispatch loop calls this inside its
    /// `with_stamped`). Off any actor's dispatch (chassis-root /
    /// injected mail) it falls back to the chassis-host ring.
    ///
    /// # Panics
    /// Panics if the chassis-host ring mutex is poisoned (fail-fast per
    /// ADR-0063) — only reachable on the off-actor fallback path.
    pub fn push_trace_ring(&self, root: MailId, event: TraceEvent) {
        // Move the event into whichever ring applies. `try_with_mut`
        // skips the closure entirely when no actor is stamped, leaving
        // `slot` populated for the chassis-host fallback — so the event
        // moves exactly once with no clone.
        let mut slot = Some(event);
        ActorTraceRing::try_with_mut(|ring| {
            if let Some(event) = slot.take() {
                ring.push(root, event);
            }
        });
        if let Some(event) = slot.take() {
            self.chassis_host_ring
                .lock()
                .expect("chassis-host trace ring mutex poisoned; fail-fast per ADR-0063")
                .push(root, event);
        }
    }

    /// Read the chassis-host ring (ADR-0086 Phase 3). The trace-tree
    /// coordinator queries the per-actor rings via `aether.trace.tail`
    /// mail, but the chassis-host ring belongs to no actor, so it is
    /// read directly through this handle. Returns the same
    /// `TraceTailResult` shape for a uniform stitch.
    ///
    /// # Panics
    /// Panics if the chassis-host ring mutex is poisoned (fail-fast per
    /// ADR-0063).
    #[must_use]
    pub fn chassis_host_tail(&self, request: &TraceTail) -> TraceTailResult {
        self.chassis_host_ring
            .lock()
            .expect("chassis-host trace ring mutex poisoned; fail-fast per ADR-0063")
            .tail(request)
    }

    /// Boot-time anchor for [`Self::now_nanos`]. Exposed for fixtures
    /// that want to reconstruct timestamps for asserting.
    #[must_use]
    pub fn boot_time(&self) -> Instant {
        self.boot_time
    }

    /// Compute the current [`Nanos`] timestamp relative to this
    /// handle's boot anchor. Sub-microsecond resolution; saturates to
    /// `u64::MAX` after ~584 years of substrate uptime.
    #[must_use]
    pub fn now_nanos(&self) -> Nanos {
        let elapsed = Instant::now().saturating_duration_since(self.boot_time);
        // u128 → u64: trace timestamps overflow after ~584 years; the
        // handle's boot anchor is set at chassis boot, so realistic
        // runtimes are well within u64 range.
        #[allow(clippy::cast_possible_truncation)]
        Nanos(elapsed.as_nanos() as u64)
    }

    /// ADR-0080 §2 producer hook for the `Sent` event. Pushes the event
    /// into the producing actor's ring (chassis-host ring off-actor) and
    /// increments the root's emit-time `in_flight` count. Stamps the
    /// `Sent` timestamp at the call (eager paths route immediately, so
    /// the call site *is* the frame-flush instant). The buffered send
    /// path splits this — [`Self::record_sent_inflight`] eagerly, then
    /// [`Self::record_sent_event_at`] at flush with the flush-begin
    /// anchor (iamacoffeepot/aether#1150).
    ///
    /// iamacoffeepot/aether#1158: eager paths route immediately, so the
    /// blob never lingered open — `t_construct_start` *is* the same `now`
    /// the `Sent` timestamp takes, making the **construct** span ≈ 0.
    pub fn record_sent(
        &self,
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        sender: MailboxId,
        recipient: MailboxId,
        kind: KindId,
    ) {
        let now = self.now_nanos();
        self.record_sent_event_at(
            mail_id,
            root,
            parent_mail,
            sender,
            recipient,
            kind,
            now,
            now,
        );
        self.record_sent_inflight(root);
    }

    /// iamacoffeepot/aether#1150: push the `Sent` trace event with an
    /// explicit timestamp, leaving the settlement counter untouched.
    /// Split from [`Self::record_sent`] so the buffered send path can
    /// defer the timestamp to flush-begin (the frame's first flush
    /// instant) — anchoring `Sent` there instead of the smeared
    /// per-send call site — while [`Self::record_sent_inflight`] keeps
    /// `in_flight` exact at send time. `t` is the frame-level flush-begin
    /// stamp; every mail in one flush shares it.
    ///
    /// iamacoffeepot/aether#1158: `t_construct_start` is the instant the
    /// producer's outbound blob opened (the first buffered send of the
    /// flush window). `t − t_construct_start` is the **construct** span;
    /// on the eager path the caller passes `t_construct_start == t`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_sent_event_at(
        &self,
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        sender: MailboxId,
        recipient: MailboxId,
        kind: KindId,
        t_construct_start: Nanos,
        t: Nanos,
    ) {
        self.push_trace_ring(
            root,
            TraceEvent::Sent {
                mail_id,
                root,
                parent_mail,
                sender,
                recipient,
                kind,
                t_construct_start,
                t,
            },
        );
    }

    /// iamacoffeepot/aether#1150: the eager half of the producer `Sent`
    /// hook — increment the root's emit-time `in_flight` count. The
    /// buffered send path calls this at send time (so settlement stays
    /// exact and never fires early, per ADR-0082) and defers the `Sent`
    /// *trace* event to flush via [`Self::record_sent_event_at`].
    pub fn record_sent_inflight(&self, root: MailId) {
        if root != MailId::NONE {
            self.settlement_counter.record_sent(root);
        }
    }

    /// ADR-0080 §2 settlement hook for the `Finished` event, called by
    /// the dispatcher trampoline at handler exit. Decrements the root's
    /// emit-time `in_flight` count and fires `Settled` on the
    /// zero-transition. (The `Finished` *trace* event is pushed
    /// separately into the recipient's ring by the dispatch loop, inside
    /// its `with_stamped` scope, so it lands in the right ring;
    /// settlement is recorded here, outside that scope, so the
    /// `fire_settled` notification — which may resolve mail subscribers
    /// inline — runs unstamped.)
    ///
    /// No-op when `mail_id == MailId::NONE` — the structural recursion
    /// break per ADR-0080 §7 for chassis-internal mail minted without
    /// lineage.
    pub fn record_finished(&self, mail_id: MailId, root: MailId) {
        if mail_id == MailId::NONE {
            return;
        }
        if root != MailId::NONE && self.settlement_counter.record_finished(root) {
            self.fire_settled(root);
        }
    }

    /// ADR-0080 §12 / iamacoffeepot/aether#716: acquire a
    /// [`SettlementHold`] against `root`. The returned guard increments
    /// the root's emit-time `held_open` count and decrements it again on
    /// `Drop`. Settlement for `root` gates on
    /// `(in_flight == 0 && held_open == 0)`, so any thread that
    /// outlives its spawning handler (`InheritCtx<A>` from
    /// [`crate::actor::native::NativeCtx::spawn_inherit`]) keeps the
    /// chain open until it drops.
    ///
    /// Acquired on the parent thread before the worker is spawned so the
    /// `held_open` increment is visible to the settlement counter before
    /// the parent handler's `Finished` lands. Moving the guard into the
    /// worker thread (via the `InheritCtx<A>` ctor) ties release to the
    /// worker's lifetime.
    #[must_use = "SettlementHold gates root settlement; storing _ silently releases it"]
    pub fn acquire_settlement_hold(&self, root: MailId) -> SettlementHold {
        if root != MailId::NONE {
            self.settlement_counter.record_hold_open(root);
        }
        SettlementHold {
            handle: self.clone(),
            root,
        }
    }
}

impl Default for TraceHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for an ADR-0080 §12 settlement hold. Construct via
/// [`TraceHandle::acquire_settlement_hold`] (or
/// [`Mailer::acquire_settlement_hold`](crate::mail::mailer::Mailer::acquire_settlement_hold));
/// drop decrements the root's `held_open` count and fires `Settled` on
/// the zero-transition. The only public surface is the guard, so a
/// paired `hold`/`release` mismatch is structurally impossible.
#[derive(Debug)]
pub struct SettlementHold {
    handle: TraceHandle,
    root: MailId,
}

impl SettlementHold {
    /// The root this hold gates. Exposed for diagnostics / tests; the
    /// hold itself releases via `Drop`.
    #[must_use]
    pub fn root(&self) -> MailId {
        self.root
    }
}

impl Drop for SettlementHold {
    fn drop(&mut self) {
        if self.root != MailId::NONE && self.handle.settlement_counter.record_release(self.root) {
            self.handle.fire_settled(self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(sender: u64, cid: u64) -> MailId {
        MailId {
            sender: MailboxId(sender),
            correlation_id: cid,
        }
    }

    /// A `TraceHandle` with a fresh registry installed, plus the registry
    /// so the test can subscribe to a root and observe the emit-time fire.
    fn handle_with_registry() -> (TraceHandle, Arc<SettlementRegistry>) {
        let handle = TraceHandle::new();
        let registry = Arc::new(SettlementRegistry::new());
        handle.install_settlement_registry(Arc::clone(&registry));
        (handle, registry)
    }

    /// ADR-0086 Phase 2: a `Sent` then its matching `Finished` drives the
    /// emit-time counter to zero and fires `Settled` *synchronously* on
    /// the calling thread — no drainer, no observer fold. The registry
    /// subscriber wakes immediately, which is the whole latency win.
    #[test]
    fn finished_zero_transition_fires_settled_synchronously() {
        let (handle, registry) = handle_with_registry();
        let root = mid(1, 7);
        let rx = registry.subscribe_settlement(root);

        handle.record_sent(root, root, None, MailboxId(1), MailboxId(2), KindId(3));
        assert!(rx.try_recv().is_err(), "must not settle before Finished");

        handle.record_finished(root, root);
        assert!(
            rx.try_recv().is_ok(),
            "emit-time counter must fire Settled on the Finished zero-transition"
        );
        assert_eq!(
            handle.settlement_counter().live_roots(),
            0,
            "cell reclaimed"
        );
    }

    /// The hold contract (ADR-0080 §12): `Finished` dropping `in_flight`
    /// to zero does NOT settle while a hold is open; only the hold's
    /// `Release` (with `in_flight` already zero) fires `Settled`.
    #[test]
    fn hold_gates_settlement_until_release() {
        let (handle, registry) = handle_with_registry();
        let root = mid(2, 9);
        let rx = registry.subscribe_settlement(root);

        let hold = handle.acquire_settlement_hold(root);
        handle.record_sent(root, root, None, MailboxId(1), MailboxId(2), KindId(3));
        handle.record_finished(root, root);
        assert!(
            rx.try_recv().is_err(),
            "an open hold must keep the root from settling"
        );

        drop(hold);
        assert!(
            rx.try_recv().is_ok(),
            "releasing the last hold fires Settled"
        );
        assert_eq!(handle.settlement_counter().live_roots(), 0);
    }

    /// `MailId::NONE` is the recursion-break sentinel and never carries
    /// settlement accounting: a NONE-rooted event must not touch the
    /// counter or fire.
    #[test]
    fn none_root_carries_no_settlement() {
        let (handle, registry) = handle_with_registry();
        let rx = registry.subscribe_settlement(MailId::NONE);
        handle.record_sent(
            MailId::NONE,
            MailId::NONE,
            None,
            MailboxId(1),
            MailboxId(2),
            KindId(3),
        );
        handle.record_finished(mid(1, 1), MailId::NONE);
        assert!(rx.try_recv().is_err(), "NONE root must never settle");
        assert_eq!(handle.settlement_counter().live_roots(), 0);
    }

    /// Before a registry is installed the zero-transition is silent (no
    /// panic): boot-time events that fire before
    /// `install_settlement_registry` simply don't notify. Boot is
    /// quiescent, so this is the trivial no-traffic case.
    #[test]
    fn fire_before_registry_install_is_silent() {
        let handle = TraceHandle::new();
        let root = mid(3, 3);
        handle.record_sent(root, root, None, MailboxId(1), MailboxId(2), KindId(3));
        handle.record_finished(root, root);
        assert_eq!(
            handle.settlement_counter().live_roots(),
            0,
            "still reclaimed"
        );
    }
}
