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
//! [`SettlementHold`] RAII guard from [`acquire_settlement_hold`]:
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
use aether_actor::{MailSender, Sender, Singleton};
use aether_data::{Kind, MailId, mailbox_id_from_name};

use super::binding::NativeBinding;
use crate::runtime::trace::{SettlementHold, acquire_settlement_hold};

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

impl<A: Actor> Sender for InheritCtx<A> {
    //noinspection DuplicatedCode
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

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        // Batch count rides as `u32` on the wire (matches the FFI ABI);
        // realistic mail batches stay well below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let count = payloads.len() as u32;
        self.binding.send_mail_with_lineage(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            bytes,
            count,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    //noinspection DuplicatedCode
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
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        <Self as Sender>::send::<R, K>(self, payload);
    }

    //noinspection DuplicatedCode
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
        // Batch count rides as `u32` on the wire (matches the FFI ABI);
        // realistic mail batches stay well below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let count = payloads.len() as u32;
        self.binding.send_mail_with_lineage(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            bytes,
            count,
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
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        <Self as Sender>::send::<R, K>(self, payload);
    }

    //noinspection DuplicatedCode
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
        Some(acquire_settlement_hold(in_flight_root))
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use aether_actor::Singleton;
    use aether_data::{KindId, MailboxId};

    use crate::mail::Mail;
    use crate::mail::registry::OwnedDispatch;
    use crate::mail::registry::Registry;
    use crate::runtime::trace;
    use crate::test_util::fresh_substrate;

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
        ids.sort_unstable();
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
                thread::sleep(Duration::from_millis(50));
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

    /// ADR-0080 §12 / iamacoffeepot/aether#716: `spawn_inherit`
    /// acquires a `SettlementHold` on the parent root before spawning
    /// and drops it on thread exit. Both events ride the global trace
    /// queue (installed by `install_trace_queue`).
    ///
    /// Test filters the queue by the unique sender mailbox id we
    /// allocate locally so the assertion is robust against other
    /// parallel tests sharing the queue (the same drain-and-restore
    /// pattern `mailer::drain_events_for` uses).
    #[test]
    fn spawn_inherit_acquires_and_releases_settlement_hold() {
        use aether_kinds::trace::TraceEvent;
        use crossbeam_queue::SegQueue;

        trace::init_substrate_start();
        let queue = Arc::new(SegQueue::<TraceEvent>::new());
        trace::install_trace_queue(Arc::clone(&queue));
        // After install_trace_queue, the global may already point at a
        // queue from a prior test. Read the live one and drain from
        // there so we observe the same queue spawn_inherit pushes to.
        let live = trace::trace_queue().expect("trace queue installed").clone();

        let (_registry, mailer) = fresh_substrate();
        let producer_mailbox = MailboxId(0xC0FE_C0FE_C0FE_C0FE);
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            producer_mailbox,
        ));
        // Root the test cares about — sender prefix is unique so we can
        // filter events out of a shared queue.
        let inherited_root = MailId::new(MailboxId(0xC0FE_C0FE_C0FE_C0FE), 9001);
        let inherited_mail_id = MailId::new(MailboxId(0xC0FE_C0FE_C0FE_C0FE), 9002);

        let join = spawn_inherit::<StubActor, _>(
            Arc::clone(&binding),
            inherited_mail_id,
            inherited_root,
            move |_inherit| {
                // No-op body — we're verifying the hold lifecycle, not
                // outbound sends.
            },
        );
        join.join().expect("inherit worker thread joins");

        // Drain queue, partition into "ours" (root matches inherited_root)
        // vs "others" (put back so other parallel tests aren't disturbed).
        let mut ours: Vec<TraceEvent> = Vec::new();
        let mut leftover: Vec<TraceEvent> = Vec::new();
        while let Some(event) = live.pop() {
            let belongs = match &event {
                TraceEvent::HoldOpen { root, .. } | TraceEvent::Release { root, .. } => {
                    *root == inherited_root
                }
                _ => false,
            };
            if belongs {
                ours.push(event);
            } else {
                leftover.push(event);
            }
        }
        for ev in leftover {
            live.push(ev);
        }

        assert_eq!(ours.len(), 2, "expected one HoldOpen + one Release");
        assert!(
            matches!(ours[0], TraceEvent::HoldOpen { root, .. } if root == inherited_root),
            "first event is HoldOpen for inherited root, got {:?}",
            ours[0]
        );
        assert!(
            matches!(ours[1], TraceEvent::Release { root, .. } if root == inherited_root),
            "second event is Release for inherited root, got {:?}",
            ours[1]
        );
    }

    /// `MailId::NONE` inherited root skips the hold — there's no chain
    /// to keep open. Verify no `HoldOpen` / `Release` events surface
    /// from a `NONE`-rooted spawn.
    #[test]
    fn spawn_inherit_with_none_root_skips_hold() {
        use aether_kinds::trace::TraceEvent;
        use crossbeam_queue::SegQueue;

        trace::init_substrate_start();
        let queue = Arc::new(SegQueue::<TraceEvent>::new());
        trace::install_trace_queue(Arc::clone(&queue));
        let live = trace::trace_queue().expect("trace queue installed").clone();

        let (_registry, mailer) = fresh_substrate();
        // Different unique sender so this test's filter doesn't catch
        // the other hold-lifecycle test's events.
        let producer_mailbox = MailboxId(0xC0FE_DEAD_C0FE_DEAD);
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            producer_mailbox,
        ));

        let join = spawn_inherit::<StubActor, _>(
            Arc::clone(&binding),
            MailId::NONE,
            MailId::NONE,
            move |_inherit| {},
        );
        join.join().expect("inherit worker thread joins");

        // Drain and inspect — there should be ZERO HoldOpen/Release
        // events for `MailId::NONE` since spawn_inherit short-circuits
        // the hold acquisition for NONE roots.
        let mut leftover: Vec<TraceEvent> = Vec::new();
        let mut none_events = 0usize;
        while let Some(event) = live.pop() {
            match &event {
                TraceEvent::HoldOpen { root, .. } | TraceEvent::Release { root, .. }
                    if *root == MailId::NONE =>
                {
                    none_events += 1;
                }
                _ => leftover.push(event),
            }
        }
        for ev in leftover {
            live.push(ev);
        }
        assert_eq!(
            none_events, 0,
            "MailId::NONE root must not produce HoldOpen/Release"
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
