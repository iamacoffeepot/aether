//! ADR-0080 §12 thread-spawn primitives.
//!
//! Caps that own threads driven by external events (TCP per-connection
//! workers, future drivers like WebSocketCapability or pollers,
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
//! ## Settlement contract gap (issue iamacoffeepot/aether#716)
//!
//! ADR-0080 §12 says "the spawning handler's tree does not settle
//! until every spawned-thread send completes." The send semantics
//! here are correct (every send carries the right `root` and
//! `parent_mail`), but the settlement contract is **not enforced**:
//! a parent chain can settle before the spawned thread's first
//! send arrives. There's no in-tree consumer surfacing this today;
//! future caps that use `spawn_inherit` for non-trivial work will
//! need the enforcement landed via the synthetic Sent/Finished
//! anchor (issue iamacoffeepot/aether#716 Option B) or the
//! `SettlementRegistry::hold_open / release` mechanism (Option C).

use std::marker::PhantomData;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use aether_actor::actor::{Actor, HandlesKind};
use aether_actor::{MailSender, Sender, Singleton};
use aether_data::{Kind, MailId, mailbox_id_from_name};

use super::binding::NativeBinding;

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
    _phantom: PhantomData<fn() -> A>,
}

impl<A> InheritCtx<A> {
    /// Construct from raw parts. Crate-private — produced only by
    /// [`super::ctx::NativeCtx::spawn_inherit`].
    pub(crate) fn new(
        binding: Arc<NativeBinding>,
        inherited_mail_id: MailId,
        inherited_root: MailId,
    ) -> Self {
        Self {
            binding,
            inherited_mail_id,
            inherited_root,
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

impl<A: Actor> Sender for InheritCtx<A> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.binding.send_mail_with_lineage(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            &bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.binding.send_mail_with_lineage(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

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
}

impl<A: Actor> MailSender for InheritCtx<A> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        <Self as Sender>::send::<R, K>(self, payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        <Self as Sender>::send_many::<R, K>(self, payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        <Self as Sender>::send_to_named::<K>(self, name, payload);
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

impl<A: Actor> Sender for RootCtx<A> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        // No inherited parent / root — each send mints its own chain
        // rooted at the freshly minted `MailId` (sender = A.mailbox).
        self.binding.send_mail_with_lineage(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            &bytes,
            1,
            None,
            None,
        );
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.binding.send_mail_with_lineage(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            None,
            None,
        );
    }

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
}

impl<A: Actor> MailSender for RootCtx<A> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        <Self as Sender>::send::<R, K>(self, payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        <Self as Sender>::send_many::<R, K>(self, payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        <Self as Sender>::send_to_named::<K>(self, name, payload);
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
    thread::Builder::new()
        .name(format!("aether-inherit-{}", A::NAMESPACE))
        .spawn(move || {
            let ctx = InheritCtx::<A>::new(binding, in_flight_mail_id, in_flight_root);
            f(ctx);
        })
        .expect("spawn aether-inherit thread")
}

/// ADR-0080 §12 thread-spawn helper. Spawns a thread carrying a
/// [`RootCtx<A>`] — each send the worker emits mints a fresh root
/// chain with `A`'s mailbox as producer.
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
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use aether_actor::Singleton;
    use aether_data::{KindId, MailboxId};

    use crate::handle_store::HandleStore;
    use crate::mail::registry::Registry;
    use crate::mail::{Mail, Mailer};

    /// Stub actor used as the `A` phantom marker on [`InheritCtx`] /
    /// [`RootCtx`]. Must impl `Singleton` because the spawn helpers
    /// require it; never instantiated.
    struct StubActor;

    impl Actor for StubActor {
        const NAMESPACE: &'static str = "test.spawn_thread.stub";
        const SCHEDULING: aether_actor::Scheduling = aether_actor::Scheduling::Dedicated;
    }

    impl Singleton for StubActor {}

    #[derive(Clone, Debug)]
    struct CapturedDispatch {
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        sender: aether_data::ReplyTo,
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
            Arc::new(move |dispatch: crate::mail::registry::OwnedDispatch| {
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
    /// This is the load-bearing semantic in PR-5-without-anchor:
    /// even though settlement may fire prematurely (issue iamacoffeepot/aether#716),
    /// the trace graph correctly attributes the spawned-thread mail
    /// to the parent chain.
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
                <InheritCtx<StubActor> as Sender>::send_to_named(
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
    /// chain — root == its own mail_id, parent_mail = None.
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
            <RootCtx<StubActor> as Sender>::send_to_named(
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
                <RootCtx<StubActor> as Sender>::send_to_named(
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
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3, "three distinct correlation ids");
    }

    /// Settlement contract gap (issue iamacoffeepot/aether#716): this
    /// test documents the known limitation. The parent chain may
    /// settle before the spawned thread sends — that's the gap.
    /// Today, the test asserts the spawned send DOES eventually
    /// happen and DOES carry the right lineage; tomorrow's
    /// settlement-anchor work flips the test to also assert the
    /// parent root's `in_flight` stays > 0 for the thread's
    /// lifetime.
    #[test]
    fn inherit_ctx_send_lineage_survives_thread_pause() {
        use std::time::Duration;

        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, "test.spawn_thread.recipient");

        let producer_mailbox = MailboxId(0xDD);
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            producer_mailbox,
        ));

        let inherited_root = MailId::new(MailboxId(0xFEED), 99);
        let inherited_mail_id = MailId::new(MailboxId(0xCAFE), 100);

        let pause_done = Arc::new(AtomicBool::new(false));
        let pause_done_clone = Arc::clone(&pause_done);

        let join = spawn_inherit::<StubActor, _>(
            Arc::clone(&binding),
            inherited_mail_id,
            inherited_root,
            move |mut inherit| {
                std::thread::sleep(Duration::from_millis(50));
                pause_done_clone.store(true, Ordering::SeqCst);
                <InheritCtx<StubActor> as Sender>::send_to_named(
                    &mut inherit,
                    "test.spawn_thread.recipient",
                    &aether_kinds::Tick,
                );
            },
        );
        join.join().expect("worker joins");
        assert!(pause_done.load(Ordering::SeqCst));

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let dispatch = &captured[0];
        assert_eq!(dispatch.root, inherited_root);
        assert_eq!(dispatch.parent_mail, Some(inherited_mail_id));
    }

    /// Setup smoke: a `Mail` pushed bare via `Mailer` doesn't trigger
    /// the spawn primitives but verifies the test fixture's
    /// register_capture closure works.
    #[test]
    fn fixture_smoke_capture_observes_bare_push() {
        let (registry, mailer) = fresh_substrate();
        let captured = register_capture(&registry, "test.spawn_thread.recipient");

        let recipient = registry
            .lookup("test.spawn_thread.recipient")
            .expect("recipient registered");
        let kind = <aether_kinds::Tick as aether_data::Kind>::ID;
        let payload = aether_data::encode_empty::<aether_kinds::Tick>();
        mailer.push(Mail::new(recipient, KindId(kind.0), payload, 1));

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        // Bare push leaves mail_id at its default (NONE).
        assert_eq!(captured[0].mail_id, MailId::NONE);
    }
}
