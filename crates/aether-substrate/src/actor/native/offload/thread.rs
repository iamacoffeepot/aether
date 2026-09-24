//! ADR-0080 §12 thread-spawn primitives.
//!
//! Caps that own threads driven by external events (TCP per-connection
//! workers, future drivers like `WebSocketCapability` or pollers,
//! occasional CPU-offload workers) need a structured way to send mail
//! from those threads while staying coherent with the trace pipeline.
//! The pattern matches the per-handler [`crate::actor::native::ctx::NativeCtx`]:
//! threads receive a ctx that grants send authority and carries the
//! inheritance choice in its type. There is no way to send mail
//! without holding one. Both ctxs send through a held proof with
//! [`MailSender::send_detached_to`]; neither carries a typed send.
//!
//! Two ctx flavours:
//!
//! - [`InheritCtx<A>`] — captures the spawning handler's in-flight
//!   `(mail_id, root)` and holds the chain open until the worker exits.
//!   Correct shape for short-burst CPU offload that is *part of* the
//!   current handler's causal closure.
//! - [`RootCtx<A>`] — no in-flight context. Each send mints a fresh
//!   root with `sender = A.mailbox` (per ADR-0080 §1 / §5). Correct
//!   shape for long-lived workers that respond to external events
//!   with no caller context — TCP per-connection workers, etc.
//!
//! ## Settlement contract (ADR-0080 §12, iamacoffeepot/aether#716)
//!
//! ADR-0080 §12 says "the spawning handler's tree does not settle
//! until every spawned-thread send completes." Enforced here via the
//! [`SettlementHold`] RAII guard from
//! [`crate::Mailer::acquire_settlement_hold`]:
//! `spawn_inherit` acquires a hold against the parent's
//! `in_flight_root` BEFORE the worker thread is spawned (so the
//! `HoldOpen` trace event lands ahead of the parent handler's
//! `Finished`), then moves the hold into the `InheritCtx<A>` so the
//! worker thread owns it. Drop fires `Release`.
//! The observer gates `Settled` emission on
//! `(in_flight == 0 && held_open == 0)`, so a worker thread that
//! outlives its handler keeps the chain open until it exits.
//!
//! A panic in either worker is fatal (ADR-0063): the body runs under
//! `fail_fast::run_or_abort` with the aborter taken before the
//! spawn, so the escalation holds whatever state the owning actor is
//! in. An unwinding `InheritCtx<A>` drops its hold before the abort.

use std::marker::PhantomData;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use aether_actor::{Addressable, ErasedActorRef, MailSender, Singleton};
use aether_data::{ActorMail, MailId};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::offload::fail_fast;
use crate::runtime::trace::SettlementHold;

/// ADR-0080 §12 spawn-context that captures the spawning handler's
/// in-flight `(mail_id, root)` and holds the parent chain open until the
/// worker exits, so spawned-thread work settles inside the parent
/// handler's causal closure. Its one send, the by-proof
/// [`MailSender::send_detached_to`], mints a fresh root.
///
/// `A` is the spawning actor's type. Held only as a phantom marker
/// for now; future work may use it to scope which actor types
/// `spawn_inherit` is available on.
pub struct InheritCtx<A> {
    binding: Arc<NativeBinding>,
    inherited_mail_id: Option<MailId>,
    inherited_root: Option<MailId>,
    /// ADR-0080 §12 settlement hold. Acquired on the parent thread
    /// before the worker is spawned (so the `HoldOpen` trace event is
    /// visible before the parent handler's `Finished` lands) and moved
    /// into the worker via this field. The hold's `Drop` impl fires
    /// `Release`, gated jointly with `in_flight` so the parent chain
    /// stays open until the worker exits. `Option` so callers without
    /// an in-flight root skip the hold cleanly — no
    /// chain to keep open.
    _hold: Option<SettlementHold>,
    _phantom: PhantomData<fn() -> A>,
}

impl<A> InheritCtx<A> {
    /// Construct from raw parts. Crate-private — produced only by
    /// [`crate::actor::native::ctx::NativeCtx::spawn_inherit`].
    pub(crate) fn new(
        binding: Arc<NativeBinding>,
        inherited_mail_id: Option<MailId>,
        inherited_root: Option<MailId>,
        hold: Option<SettlementHold>,
    ) -> Self {
        Self { binding, inherited_mail_id, inherited_root, _hold: hold, _phantom: PhantomData }
    }

    /// The in-flight `MailId` this ctx inherited from its spawning
    /// handler, `None` when it had none.
    #[must_use]
    pub fn inherited_mail_id(&self) -> Option<MailId> {
        self.inherited_mail_id
    }

    /// The chain root this ctx inherited from its spawning handler, whose
    /// settlement the ctx holds open; `None` when the spawning handler ran
    /// in no chain.
    #[must_use]
    pub fn inherited_root(&self) -> Option<MailId> {
        self.inherited_root
    }
}

impl<A: Addressable> MailSender for InheritCtx<A> {
    fn prev_correlation(&self) -> u64 {
        self.binding.prev_correlation()
    }

    // By-id detached send: `None` / `None` lineage mints a fresh root
    // rather than inheriting this ctx's captured chain (ADR-0080 §7).
    fn send_detached_to<K: ActorMail>(&mut self, target: ErasedActorRef, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.binding.send_mail_with_lineage(target.id().0, K::ID.0, &bytes, 1, None, None);
    }
}

/// ADR-0080 §12 spawn-context with no in-flight inheritance. Each
/// outbound send mints a fresh root chain with the spawning actor's
/// mailbox as producer (per ADR-0080 §1 / §5). Correct shape for
/// long-lived workers that respond to external events with no
/// caller-supplied causal context — TCP per-connection workers, future
/// pollers, etc. Its one send is the by-proof
/// [`MailSender::send_detached_to`].
pub struct RootCtx<A> {
    binding: Arc<NativeBinding>,
    _phantom: PhantomData<fn() -> A>,
}

impl<A> RootCtx<A> {
    /// Construct from raw parts. Crate-private — produced only by
    /// [`crate::actor::native::ctx::NativeCtx::spawn_detached`].
    pub(crate) fn new(binding: Arc<NativeBinding>) -> Self {
        Self { binding, _phantom: PhantomData }
    }
}

impl<A: Addressable> MailSender for RootCtx<A> {
    fn prev_correlation(&self) -> u64 {
        self.binding.prev_correlation()
    }

    // By-id detached send. A root ctx has no captured chain, so every send
    // mints a fresh root with `None` / `None` lineage.
    fn send_detached_to<K: ActorMail>(&mut self, target: ErasedActorRef, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.binding.send_mail_with_lineage(target.id().0, K::ID.0, &bytes, 1, None, None);
    }
}

/// ADR-0080 §12 thread-spawn helper. Spawns a thread carrying an
/// [`InheritCtx<A>`] that captures `(in_flight_mail_id, in_flight_root)`
/// from the spawning handler. The spawning function is the
/// `spawn_inherit` entry point on
/// [`crate::actor::native::ctx::NativeCtx`]; this function is the crate-private
/// runtime body it delegates to.
// This IS the spawn_inherit primitive (ADR-0080 §12) — the sanctioned raw spawn
// the lint points callers at; it cannot route through itself.
#[allow(clippy::disallowed_methods)]
pub(crate) fn spawn_inherit<A, F>(
    binding: Arc<NativeBinding>,
    in_flight_mail_id: Option<MailId>,
    in_flight_root: Option<MailId>,
    f: F,
) -> JoinHandle<()>
where
    // ADR-0119: `A` is used only for `A::NAMESPACE` + `InheritCtx<A>`
    // (Addressable-only); the former `Singleton` bound was incidental.
    A: Addressable + 'static,
    F: FnOnce(InheritCtx<A>) + Send + 'static,
{
    // ADR-0080 §12 / iamacoffeepot/aether#716: acquire the settlement
    // hold on the parent thread BEFORE spawning. The `HoldOpen` event
    // hits the trace queue ahead of the parent handler's `Finished`,
    // so by the time the observer sees `in_flight` reach zero the
    // `held_open` counter is already non-zero. Move the hold into the
    // spawned closure via the `InheritCtx<A>` so release fires on
    // worker exit.
    //
    // A ctx without an in-flight root has no chain to keep open, so the
    // acquire hands back no hold.
    let hold = in_flight_root.map(|root| binding.mailer().acquire_settlement_hold(root));
    let aborter = binding.fatal_aborter();
    thread::Builder::new()
        .name(format!("aether-inherit-{}", A::NAMESPACE))
        .spawn(move || {
            let ctx = InheritCtx::<A>::new(binding, in_flight_mail_id, in_flight_root, hold);
            fail_fast::run_or_abort(aborter.as_ref(), "spawn_inherit worker", move || f(ctx));
        })
        .expect("spawn aether-inherit thread")
}

/// ADR-0080 §12 thread-spawn helper. Spawns a thread carrying a
/// [`RootCtx<A>`] — each send the worker emits mints a fresh root
/// chain with `A`'s mailbox as producer.
// This IS the spawn_detached primitive (ADR-0080 §12) — the sanctioned raw spawn
// the lint points callers at; it cannot route through itself.
#[allow(clippy::disallowed_methods)]
pub(crate) fn spawn_detached<A, F>(binding: Arc<NativeBinding>, f: F) -> JoinHandle<()>
where
    A: Addressable + Singleton + 'static,
    F: FnOnce(RootCtx<A>) + Send + 'static,
{
    let aborter = binding.fatal_aborter();
    thread::Builder::new()
        .name(format!("aether-root-{}", A::NAMESPACE))
        .spawn(move || {
            let ctx = RootCtx::<A>::new(binding);
            fail_fast::run_or_abort(aborter.as_ref(), "spawn_detached worker", move || f(ctx));
        })
        .expect("spawn aether-root thread")
}

#[cfg(test)]
// Test helpers use `Mutex<Vec<...>>` as a capture buffer; the guard
// is held through `.push(...)` which is the captured payload — that's
// the intended sequence, not a tightening opportunity.
#[allow(clippy::significant_drop_tightening)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and capture panic on failure is the assertion"
)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use aether_data::{Kind, KindId, MailboxId};

    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::mail::{Mail, Mailer};
    use crate::runtime::panic_hook::payload_string;
    use crate::testing::boot_authority;

    /// Stub actor used as the `A` phantom marker on [`InheritCtx`] /
    /// [`RootCtx`]. Must impl `Singleton` because the spawn helpers
    /// require it; never instantiated.
    struct StubActor;

    impl Addressable for StubActor {
        const NAMESPACE: &'static str = "test.spawn_thread.stub";
        type Resolver = aether_actor::One;
    }

    #[derive(Clone, Debug)]
    struct CapturedDispatch {
        mail_id: Option<MailId>,
        root: Option<MailId>,
        parent_mail: Option<MailId>,
    }

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        (registry, mailer)
    }

    fn register_capture(registry: &Registry, name: &str) -> Arc<Mutex<Vec<CapturedDispatch>>> {
        let captured: Arc<Mutex<Vec<CapturedDispatch>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        // iamacoffeepot/aether#848 PR 3: synchronous lineage-only
        // capture; take `OwnedDispatch` directly (no envelope build,
        // no `to_vec()` clone on the lineage fields which are all
        // Copy).
        let _ = registry.try_register_inbox(
            &boot_authority(),
            name.to_owned(),
            Arc::new(move |dispatch: OwnedDispatch| {
                // ADR-0094: terminal test consumer — discharge the
                // obligation it captures.
                dispatch.discharge();
                captured_for_handler.lock().unwrap().push(CapturedDispatch {
                    mail_id: dispatch.mail_id,
                    root: dispatch.root,
                    parent_mail: dispatch.parent_mail,
                });
            }),
        );
        captured
    }

    /// A detached send through a proof from `InheritCtx` cuts the captured
    /// lineage and mints a fresh root.
    #[test]
    fn inherit_ctx_detached_send_mints_fresh_root() {
        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, StubActor::NAMESPACE);
        let target = Registry::structural_erased(registry.lookup(StubActor::NAMESPACE).expect("capture registered"));

        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0xAB)));
        let inherited_root = MailId::new(MailboxId(0x1234), 7);
        let inherited_mail_id = MailId::new(MailboxId(0x5678), 13);

        let join = spawn_inherit::<StubActor, _>(
            binding,
            Some(inherited_mail_id),
            Some(inherited_root),
            move |mut inherit| {
                inherit.send_detached_to(target, &aether_kinds::Tick::default());
            },
        );
        join.join().expect("inherit worker thread joins");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "one detached mail dispatched");
        for dispatch in captured.iter() {
            assert_eq!(dispatch.parent_mail, None, "detached send has no parent");
            assert_eq!(dispatch.root, dispatch.mail_id, "detached send is its own root");
        }
    }

    /// A detached send through a proof from `RootCtx` mints a fresh root
    /// with no parent.
    #[test]
    fn root_ctx_detached_send_mints_fresh_root() {
        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, StubActor::NAMESPACE);
        let target = Registry::structural_erased(registry.lookup(StubActor::NAMESPACE).expect("capture registered"));

        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0xBC)));
        let join = spawn_detached::<StubActor, _>(binding, move |mut root| {
            root.send_detached_to(target, &aether_kinds::Tick::default());
        });
        join.join().expect("root worker thread joins");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "one detached mail dispatched");
        for dispatch in captured.iter() {
            assert_eq!(dispatch.parent_mail, None, "detached send has no parent");
            assert_eq!(dispatch.root, dispatch.mail_id, "detached send is its own root");
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
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), producer_mailbox));
        let inherited_root = MailId::new(MailboxId(0xC0FE_C0FE_C0FE_C0FE), 9001);
        let inherited_mail_id = MailId::new(MailboxId(0xC0FE_C0FE_C0FE_C0FE), 9002);

        // Gate the worker so the hold stays open across the assertion.
        let (gate_tx, gate_rx) = channel::<()>();
        let join = spawn_inherit::<StubActor, _>(
            Arc::clone(&binding),
            Some(inherited_mail_id),
            Some(inherited_root),
            move |_inherit| {
                // Block until released — the SettlementHold (moved into
                // this worker's InheritCtx) is held for the whole body.
                let _ = gate_rx.recv();
            },
        );

        // The hold is acquired on the parent thread before the spawn, so
        // it is open now regardless of worker scheduling.
        assert_eq!(
            counter.held_open(inherited_root),
            1,
            "spawn_inherit must acquire a settlement hold on the inherited root"
        );

        gate_tx.send(()).expect("release worker");
        join.join().expect("inherit worker thread joins");

        // The InheritCtx dropped on worker exit → hold released → the
        // (0, 0) cell is reclaimed.
        assert_eq!(counter.held_open(inherited_root), 0, "the hold must release when the worker exits");
    }

    /// A panic in a `spawn_inherit` worker escalates through the binding's
    /// chassis aborter with the payload in the reason (ADR-0063), and the
    /// unwinding worker drops its settlement hold on the way out. The test
    /// binding's aborter is `PanicAborter`, so the joined payload is the
    /// aborter's own message; a worker run bare joins with the probe alone.
    #[test]
    fn spawn_inherit_worker_panic_escalates_through_the_aborter() {
        let (_registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x6431)));
        let inherited_root = MailId::new(MailboxId(0x6431), 1);
        let inherited_mail_id = MailId::new(MailboxId(0x6431), 2);

        let payload =
            spawn_inherit::<StubActor, _>(binding, Some(inherited_mail_id), Some(inherited_root), |_inherit| {
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
        assert_eq!(counter.held_open(inherited_root), 0, "the unwinding worker released its hold");
    }

    /// A panic in a `spawn_detached` worker escalates through the binding's
    /// chassis aborter with the payload in the reason (ADR-0063).
    #[test]
    fn spawn_detached_worker_panic_escalates_through_the_aborter() {
        let (_registry, mailer) = fresh_substrate();
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x6432)));

        let payload = spawn_detached::<StubActor, _>(binding, |_root| {
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
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), producer_mailbox));

        let live_before = counter.live_roots();
        let join = spawn_inherit::<StubActor, _>(Arc::clone(&binding), None, None, move |_inherit| {});
        join.join().expect("inherit worker thread joins");

        assert_eq!(counter.live_roots(), live_before, "a rootless spawn must not create a settlement cell");
    }

    /// Setup smoke: a `Mail` pushed bare via `Mailer` doesn't trigger
    /// the spawn primitives but verifies the test fixture's
    /// `register_capture` closure works.
    #[test]
    fn fixture_smoke_capture_observes_bare_push() {
        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, "test.spawn_thread.recipient");

        let recipient = registry.lookup("test.spawn_thread.recipient").expect("recipient registered");
        let kind = <aether_kinds::Tick as Kind>::ID;
        let payload = aether_kinds::Tick::default().encode_into_bytes();
        mailer.push(Mail::new(recipient, KindId(kind.0), payload, 1));

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        // Bare push stamps no mail id.
        assert_eq!(captured[0].mail_id, None);
    }
}
