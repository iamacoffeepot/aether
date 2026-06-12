//! ADR-0080 §12 thread-spawn primitives.
//!
//! Caps that own threads driven by external events (TCP per-connection
//! workers, future drivers like `WebSocketCapability` or pollers,
//! occasional CPU-offload workers) need a structured way to send mail
//! from those threads while staying coherent with the trace pipeline.
//! The pattern matches the per-handler [`super::ctx::NativeCtx`]:
//! threads receive a ctx that grants send authority and carries the
//! inheritance choice in its type. There is no way to send mail
//! without holding one.
//!
//! Two ctx flavours:
//!
//! - [`InheritCtx<A>`] — captures the spawning handler's in-flight
//!   `(mail_id, root)`. Sends inherit `root` and stamp
//!   `parent_mail = self.in_flight.mail_id`. Correct shape for
//!   short-burst CPU offload that is *part of* the current handler's
//!   causal closure.
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

use std::marker::PhantomData;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use aether_actor::actor::{Actor, HandlesKind};
use aether_actor::{MailSender, Singleton};
use aether_data::{Kind, MailId, mailbox_id_from_name};

use super::binding::NativeBinding;
use crate::runtime::trace::SettlementHold;

/// ADR-0080 §12 spawn-context that captures the spawning handler's
/// in-flight `(mail_id, root)`. Outbound sends from the worker
/// thread inherit the parent root and stamp `parent_mail` to the
/// in-flight mail id, so spawned-thread work folds into the parent
/// handler's causal chain in the trace graph.
///
/// `A` is the spawning actor's type. Held only as a phantom marker
/// for now; future work may use it to scope which actor types
/// `spawn_inherit` is available on.
pub struct InheritCtx<A> {
    binding: Arc<NativeBinding>,
    inherited_mail_id: MailId,
    inherited_root: MailId,
    /// ADR-0080 §12 settlement hold. Acquired on the parent thread
    /// before the worker is spawned (so the `HoldOpen` trace event is
    /// visible before the parent handler's `Finished` lands) and moved
    /// into the worker via this field. The hold's `Drop` impl fires
    /// `Release`, gated jointly with `in_flight` so the parent chain
    /// stays open until the worker exits. `Option` so callers without
    /// an in-flight root (`MailId::NONE`) skip the hold cleanly — no
    /// chain to keep open.
    _hold: Option<SettlementHold>,
    _phantom: PhantomData<fn() -> A>,
}

impl<A> InheritCtx<A> {
    /// Construct from raw parts. Crate-private — produced only by
    /// [`super::ctx::NativeCtx::spawn_inherit`].
    pub(crate) fn new(
        binding: Arc<NativeBinding>,
        inherited_mail_id: MailId,
        inherited_root: MailId,
        hold: Option<SettlementHold>,
    ) -> Self {
        Self {
            binding,
            inherited_mail_id,
            inherited_root,
            _hold: hold,
            _phantom: PhantomData,
        }
    }

    /// The in-flight `MailId` this ctx inherited from its spawning
    /// handler. Outbound sends use this as `parent_mail`.
    #[must_use]
    pub fn inherited_mail_id(&self) -> MailId {
        self.inherited_mail_id
    }

    /// The chain root this ctx inherited from its spawning handler.
    /// Outbound sends inherit this as their `root`.
    #[must_use]
    pub fn inherited_root(&self) -> MailId {
        self.inherited_root
    }

    fn outbound_parent(&self) -> Option<MailId> {
        if self.inherited_mail_id == MailId::NONE {
            None
        } else {
            Some(self.inherited_mail_id)
        }
    }

    fn outbound_root(&self) -> Option<MailId> {
        if self.inherited_root == MailId::NONE {
            None
        } else {
            Some(self.inherited_root)
        }
    }
}

impl<A: Actor> MailSender for InheritCtx<A> {
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.binding.send_mail_with_lineage(
            R::resolve(self.binding.carry()).0,
            K::ID.0,
            &bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        // Batch count rides as `u32` on the wire (matches the FFI ABI);
        // realistic mail batches stay well below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let count = payloads.len() as u32;
        self.binding.send_mail_with_lineage(
            R::resolve(self.binding.carry()).0,
            K::ID.0,
            bytes,
            count,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name send escape hatch (the `Resolver::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.binding.send_mail_with_lineage(
            mailbox_id_from_name(name).0,
            K::ID.0,
            &bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    fn prev_correlation(&self) -> u64 {
        self.binding.prev_correlation()
    }
}

/// ADR-0080 §12 spawn-context with no in-flight inheritance. Each
/// outbound send mints a fresh root chain with the spawning actor's
/// mailbox as producer (per ADR-0080 §1 / §5). Correct shape for
/// long-lived workers that respond to external events with no
/// caller-supplied causal context — TCP per-connection workers, future
/// pollers, etc.
pub struct RootCtx<A> {
    binding: Arc<NativeBinding>,
    _phantom: PhantomData<fn() -> A>,
}

impl<A> RootCtx<A> {
    /// Construct from raw parts. Crate-private — produced only by
    /// [`super::ctx::NativeCtx::spawn_detached`].
    pub(crate) fn new(binding: Arc<NativeBinding>) -> Self {
        Self {
            binding,
            _phantom: PhantomData,
        }
    }
}

impl<A: Actor> MailSender for RootCtx<A> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        // No inherited parent / root — each send mints its own chain
        // rooted at the freshly minted `MailId` (sender = A.mailbox).
        self.binding.send_mail_with_lineage(
            R::resolve(self.binding.carry()).0,
            K::ID.0,
            &bytes,
            1,
            None,
            None,
        );
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        // Batch count rides as `u32` on the wire (matches the FFI ABI);
        // realistic mail batches stay well below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let count = payloads.len() as u32;
        self.binding.send_mail_with_lineage(
            R::resolve(self.binding.carry()).0,
            K::ID.0,
            bytes,
            count,
            None,
            None,
        );
    }

    // Runtime-name send escape hatch (the `Resolver::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.binding.send_mail_with_lineage(
            mailbox_id_from_name(name).0,
            K::ID.0,
            &bytes,
            1,
            None,
            None,
        );
    }

    fn prev_correlation(&self) -> u64 {
        self.binding.prev_correlation()
    }
}

/// ADR-0080 §12 thread-spawn helper. Spawns a thread carrying an
/// [`InheritCtx<A>`] that captures `(in_flight_mail_id, in_flight_root)`
/// from the spawning handler. The spawning function is the
/// `spawn_inherit` entry point on
/// [`super::ctx::NativeCtx`]; this function is the crate-private
/// runtime body it delegates to.
// This IS the spawn_inherit primitive (ADR-0080 §12) — the sanctioned raw spawn
// the lint points callers at; it cannot route through itself.
#[allow(clippy::disallowed_methods)]
pub(crate) fn spawn_inherit<A, F>(
    binding: Arc<NativeBinding>,
    in_flight_mail_id: MailId,
    in_flight_root: MailId,
    f: F,
) -> JoinHandle<()>
where
    A: Actor + Singleton + 'static,
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
    // `MailId::NONE` skips the hold: a ctx without an in-flight root
    // has no chain to keep open. Symmetric with the `outbound_root`
    // / `outbound_parent` `None` cases.
    let hold = if in_flight_root == MailId::NONE {
        None
    } else {
        Some(binding.mailer().acquire_settlement_hold(in_flight_root))
    };
    thread::Builder::new()
        .name(format!("aether-inherit-{}", A::NAMESPACE))
        .spawn(move || {
            let ctx = InheritCtx::<A>::new(binding, in_flight_mail_id, in_flight_root, hold);
            f(ctx);
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
    A: Actor + Singleton + 'static,
    F: FnOnce(RootCtx<A>) + Send + 'static,
{
    thread::Builder::new()
        .name(format!("aether-root-{}", A::NAMESPACE))
        .spawn(move || {
            let ctx = RootCtx::<A>::new(binding);
            f(ctx);
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

    use aether_actor::Singleton;
    use aether_data::{KindId, MailboxId};

    use crate::handle_store::HandleStore;
    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::mail::{Mail, Mailer};

    /// Stub actor used as the `A` phantom marker on [`InheritCtx`] /
    /// [`RootCtx`]. Must impl `Singleton` because the spawn helpers
    /// require it; never instantiated.
    struct StubActor;

    impl Actor for StubActor {
        const NAMESPACE: &'static str = "test.spawn_thread.stub";
    }

    impl Singleton for StubActor {}

    #[derive(Clone, Debug)]
    struct CapturedDispatch {
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        sender: aether_data::Source,
    }

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
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
            name.to_owned(),
            Arc::new(move |dispatch: OwnedDispatch| {
                // ADR-0094: terminal test consumer — discharge the
                // obligation it captures.
                dispatch.discharge();
                captured_for_handler.lock().unwrap().push(CapturedDispatch {
                    mail_id: dispatch.mail_id,
                    root: dispatch.root,
                    parent_mail: dispatch.parent_mail,
                    sender: dispatch.sender,
                });
            }),
        );
        captured
    }

    /// `InheritCtx`-spawned thread's `send_to_named` carries the
    /// inherited root and stamps `parent_mail = inherited_mail_id`.
    /// Settlement is held open until the worker thread exits per
    /// ADR-0080 §12 / iamacoffeepot/aether#716; see
    /// `spawn_inherit_acquires_and_releases_settlement_hold` below
    /// for the held-open verification.
    #[test]
    fn inherit_ctx_send_carries_root_and_parent_mail() {
        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, "test.spawn_thread.recipient");

        let producer_mailbox = MailboxId(0xAA);
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            producer_mailbox,
        ));

        let inherited_root = MailId::new(MailboxId(0x1234), 7);
        let inherited_mail_id = MailId::new(MailboxId(0x5678), 13);

        let join = spawn_inherit::<StubActor, _>(
            Arc::clone(&binding),
            inherited_mail_id,
            inherited_root,
            move |mut inherit| {
                <InheritCtx<StubActor> as MailSender>::send_to_named(
                    &mut inherit,
                    "test.spawn_thread.recipient",
                    &aether_kinds::Tick,
                );
            },
        );
        join.join().expect("inherit worker thread joins");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "exactly one mail dispatched");
        let dispatch = &captured[0];
        assert_eq!(
            dispatch.root, inherited_root,
            "spawned-thread send inherits parent root"
        );
        assert_eq!(
            dispatch.parent_mail,
            Some(inherited_mail_id),
            "spawned-thread send stamps parent_mail = inherited mail_id"
        );
        // `mail_id.sender` is the producer (the actor's binding mailbox);
        // `mail_id.correlation_id` came from the binding's per-actor counter.
        assert_eq!(
            dispatch.mail_id.sender, producer_mailbox,
            "fresh mail_id carries the binding's actor mailbox as producer"
        );
        assert!(
            dispatch.mail_id.correlation_id > 0,
            "fresh mail_id has a non-zero correlation"
        );
    }

    /// `RootCtx`-spawned thread's `send_to_named` mints a fresh root
    /// chain — root == its own `mail_id`, `parent_mail` = None.
    #[test]
    fn root_ctx_send_mints_fresh_root_with_no_parent() {
        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, "test.spawn_thread.recipient");

        let producer_mailbox = MailboxId(0xBB);
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            producer_mailbox,
        ));

        let join = spawn_detached::<StubActor, _>(Arc::clone(&binding), move |mut root| {
            <RootCtx<StubActor> as MailSender>::send_to_named(
                &mut root,
                "test.spawn_thread.recipient",
                &aether_kinds::Tick,
            );
        });
        join.join().expect("root worker thread joins");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "exactly one mail dispatched");
        let dispatch = &captured[0];
        assert_eq!(
            dispatch.parent_mail, None,
            "RootCtx send has no parent — chassis-root style"
        );
        assert_eq!(
            dispatch.root, dispatch.mail_id,
            "RootCtx send is its own root"
        );
        assert_eq!(
            dispatch.mail_id.sender, producer_mailbox,
            "fresh mail_id carries the binding's actor mailbox as producer"
        );
        let _ = dispatch.sender;
    }

    /// Multiple `RootCtx` sends each mint independent root chains.
    /// The `mail_id` correlation counter advances; each send's `root`
    /// equals its own `mail_id` (a chain of one).
    #[test]
    fn root_ctx_each_send_is_an_independent_root() {
        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, "test.spawn_thread.recipient");

        let producer_mailbox = MailboxId(0xCC);
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            producer_mailbox,
        ));

        let join = spawn_detached::<StubActor, _>(Arc::clone(&binding), move |mut root| {
            for _ in 0..3 {
                <RootCtx<StubActor> as MailSender>::send_to_named(
                    &mut root,
                    "test.spawn_thread.recipient",
                    &aether_kinds::Tick,
                );
            }
        });
        join.join().expect("root worker thread joins");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 3);
        for d in captured.iter() {
            assert_eq!(d.root, d.mail_id, "each send is its own root");
            assert_eq!(d.parent_mail, None);
        }
        // Correlation ids are monotonic per actor — three sends, three
        // distinct values.
        let mut ids: Vec<u64> = captured.iter().map(|d| d.mail_id.correlation_id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3, "three distinct correlation ids");
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
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            producer_mailbox,
        ));
        let inherited_root = MailId::new(MailboxId(0xC0FE_C0FE_C0FE_C0FE), 9001);
        let inherited_mail_id = MailId::new(MailboxId(0xC0FE_C0FE_C0FE_C0FE), 9002);

        // Gate the worker so the hold stays open across the assertion.
        let (gate_tx, gate_rx) = channel::<()>();
        let join = spawn_inherit::<StubActor, _>(
            Arc::clone(&binding),
            inherited_mail_id,
            inherited_root,
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
        assert_eq!(
            counter.held_open(inherited_root),
            0,
            "the hold must release when the worker exits"
        );
    }

    /// `MailId::NONE` inherited root skips the hold — there's no chain to
    /// keep open. Verify a `NONE`-rooted spawn creates no settlement cell.
    #[test]
    fn spawn_inherit_with_none_root_skips_hold() {
        let (_registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let producer_mailbox = MailboxId(0xC0FE_DEAD_C0FE_DEAD);
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            producer_mailbox,
        ));

        let live_before = counter.live_roots();
        let join = spawn_inherit::<StubActor, _>(
            Arc::clone(&binding),
            MailId::NONE,
            MailId::NONE,
            move |_inherit| {},
        );
        join.join().expect("inherit worker thread joins");

        assert_eq!(
            counter.held_open(MailId::NONE),
            0,
            "MailId::NONE root must not acquire a settlement hold"
        );
        assert_eq!(
            counter.live_roots(),
            live_before,
            "a NONE-root spawn must not create a settlement cell"
        );
    }

    /// Setup smoke: a `Mail` pushed bare via `Mailer` doesn't trigger
    /// the spawn primitives but verifies the test fixture's
    /// `register_capture` closure works.
    #[test]
    fn fixture_smoke_capture_observes_bare_push() {
        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, "test.spawn_thread.recipient");

        let recipient = registry
            .lookup("test.spawn_thread.recipient")
            .expect("recipient registered");
        let kind = <aether_kinds::Tick as Kind>::ID;
        let payload = aether_data::encode_empty::<aether_kinds::Tick>();
        mailer.push(Mail::new(recipient, KindId(kind.0), payload, 1));

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        // Bare push leaves mail_id at its default (NONE).
        assert_eq!(captured[0].mail_id, MailId::NONE);
    }
}
