//! The `with_actor` singleton path end to end: boot, dispatch, teardown, and the
//! per-actor slots the trampoline stamps into TLS across `init` and each
//! handler call.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::registry;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Issue 552 stage 1: end-to-end smoke for the new
/// [`Builder::with_actor`] boot path. Boots a hand-rolled
/// `NativeActor` fixture, looks it up via
/// [`PassiveChassis::actor`], pushes one envelope at the cap's
/// mailbox, and asserts the dispatcher routed it to the right
/// handler. Stage 1 lands the infrastructure; stage 2 migrates
/// real caps onto it. This test is the load-bearing acceptance
/// gate.
#[test]
fn with_actor_boots_dispatches_and_tears_down() {
    use crate::mail::registry::MailboxEntry;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // Fixture kind: a 4-byte cast-shape payload so encode_into_bytes
    // lands on the bytemuck path.
    pod_kind!(Ping { tag: u32 }, "test.with_actor.ping", 0xA1B2_C3D4_E5F6_0001);

    // Fixture cap. State behind interior mutability so `&self`
    // dispatch can mutate it (the post-552 norm).
    struct ProbeCap {
        received: Arc<AtomicU32>,
    }
    impl Addressable for ProbeCap {
        const NAMESPACE: &'static str = "test.with_actor.probe";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for ProbeCap {}
    impl HandlesKind<Ping> for ProbeCap {}

    impl aether_actor::Lifecycle<Self> for ProbeCap {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received: params })
        }
    }

    impl NativeActor for ProbeCap {
        type State = Self;
    }

    // Hand-rolled Dispatch — what the macro arm emits in
    // task #731. The if-arm decodes Ping bytes, calls the
    // handler, returns Some(()) on success.
    impl Dispatch<Self> for ProbeCap {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Ping::ID.0 {
                let _decoded = Ping::decode_from_bytes(payload)?;
                state.received.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let received = Arc::new(AtomicU32::new(0));

    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<ProbeCap>(Arc::clone(&received))
        .build_passive()
        .expect("with_actor boot succeeds");

    // Issue 629 / Phase A: chassis-level `actor::<X>()` retired.
    // The cap is owned by its dispatcher thread; the test verifies
    // the cap is alive via the mail dispatch round-trip below.

    // Push one envelope at the cap's mailbox via the registry's
    // sink handler. The dispatcher thread pulls from its inbox
    // and routes through __aether_dispatch_envelope → on_ping.
    let mailbox_id = registry.lookup(<ProbeCap as Addressable>::NAMESPACE).expect("with_actor claimed the mailbox");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(mailbox_id).expect("sink registered") else {
        panic!("ProbeCap claim must be a sink entry");
    };

    let payload = Ping { tag: 0xDEAD_BEEF };
    let bytes = payload.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(<Ping as Kind>::ID, &bytes, 1));

    // Wait briefly for the dispatcher thread to dispatch.
    let deadline = Instant::now() + Duration::from_millis(500);
    while received.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        received.load(AtomicOrdering::SeqCst),
        1,
        "dispatcher should have routed Ping → on_ping within the wait budget"
    );

    drop(chassis);
}

/// Issue 582: the chassis dispatcher trampoline stamps the
/// per-actor [`ActorSlots`] into TLS
/// for the duration of `init` and each handler call. A cap that
/// reaches for `Local::with_mut` from inside both lifecycle
/// stages must see its own state — verified end-to-end here so
/// the stamping wiring can't silently regress.
#[test]
fn with_actor_stamps_local_for_init_and_handler() {
    use crate::mail::registry::MailboxEntry;
    use aether_actor::Local;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Tick { seq: u32 }, "test.local.tick", 0xA1B2_C3D4_E5F6_0002);

    // The cap holds an Arc<AtomicU32> the test reads after each
    // dispatch. The actor-local counter is keyed by `TypeId<Counter>`
    // — the chassis stamp is what makes `with_mut` resolve at
    // all (outside a stamp it would `debug_assert!` panic).
    struct LocalProbe {
        observed: Arc<AtomicU32>,
    }
    impl Addressable for LocalProbe {
        const NAMESPACE: &'static str = "test.local.probe";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for LocalProbe {}
    impl HandlesKind<Tick> for LocalProbe {}

    // Newtype-per-slot is the Local convention: each
    // logical storage gets its own type, so two probes that
    // both want a u32 don't alias under TypeId. The
    // `#[local]` attribute is the shorthand for the
    // marker impl.
    #[derive(Default)]
    #[aether_actor::local]
    struct Counter(u32);

    impl aether_actor::Lifecycle<Self> for LocalProbe {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            // Init runs inside the chassis builder's stamp guard
            // — write a sentinel so the handler test below proves
            // the same slots are reused across init→dispatch.
            Counter::with_mut(|c| c.0 = 100);
            Ok(Self { observed: params })
        }
    }

    impl NativeActor for LocalProbe {
        type State = Self;
    }

    impl Dispatch<Self> for LocalProbe {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Tick::ID.0 {
                let _decoded = Tick::decode_from_bytes(payload)?;
                Counter::with_mut(|c| c.0 += 1);
                let snapshot = Counter::with(|c| c.0);
                state.observed.store(snapshot, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let observed = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<LocalProbe>(Arc::clone(&observed))
        .build_passive()
        .expect("LocalProbe boots");

    let mailbox_id = registry.lookup(<LocalProbe as Addressable>::NAMESPACE).expect("with_actor claimed the mailbox");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(mailbox_id).expect("sink registered") else {
        panic!("LocalProbe claim must be a sink entry");
    };

    // Three dispatches. Init seeded 100; the handler bumps once
    // per dispatch and snapshots — so observed should walk
    // 101, 102, 103 in order. We assert the final 103 with a
    // wait budget to cover dispatcher-thread scheduling.
    for seq in 0..3 {
        let payload = Tick { seq };
        let bytes = payload.encode_into_bytes();
        handler.enqueue(registry::test_owned_dispatch(<Tick as Kind>::ID, &bytes, 1));
    }

    let deadline = Instant::now() + Duration::from_millis(500);
    while observed.load(AtomicOrdering::SeqCst) != 103 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        observed.load(AtomicOrdering::SeqCst),
        103,
        "init seeded 100 + 3 handler bumps ⇒ Local at 103 (proves the same \
         ActorSlots is stamped across init and dispatch)"
    );

    drop(chassis);
}
