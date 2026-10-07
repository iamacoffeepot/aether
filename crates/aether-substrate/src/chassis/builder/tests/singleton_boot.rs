//! The `with_actor` singleton path end to end: boot, dispatch, teardown, and the
//! per-actor slots the trampoline stamps into TLS across `init` and each
//! handler call.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::testing::{TestChassis, await_settled, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

/// Issue 552 stage 1: end-to-end smoke for the new
/// [`Builder::with_actor`] boot path. Boots a hand-rolled
/// `NativeActor` fixture, sends one tracked mail through its
/// composed reference, and asserts the dispatcher routed it to the right handler.
/// Stage 1 lands the infrastructure; stage 2 migrates
/// real caps onto it. This test is the load-bearing acceptance
/// gate.
#[test]
fn with_actor_boots_dispatches_and_tears_down() {
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
    impl HandlesKind<Ping> for ProbeCap {
        type Sender = aether_actor::Anyone;
    }

    impl aether_actor::Lifecycle<Self> for ProbeCap {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received: params })
        }
    }

    impl aether_actor::Declared for ProbeCap {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
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
            _ctx: &mut NativeCtx<'_, Self, crate::Anyone, crate::Unchecked>,
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

    // Send one tracked mail through the cap's composed reference.
    // The dispatcher pulls from its inbox and routes through
    // __aether_dispatch_envelope → on_ping before the root settles.
    let (_, settled) = chassis.send_tracked(chassis.actor_ref::<ProbeCap>(), &Ping { tag: 0xDEAD_BEEF }, None);
    await_settled(&settled, "test.with_actor.ping");
    assert_eq!(received.load(AtomicOrdering::SeqCst), 1, "dispatcher should have routed Ping → on_ping");

    drop(chassis);
}

/// Issue 582: the chassis dispatcher trampoline stamps the
/// per-actor [`ActorSlots`](aether_actor::local::ActorSlots) into TLS
/// for the duration of `init` and each handler call. A cap that
/// reaches for `Local::with_mut` from inside both lifecycle
/// stages must see its own state — verified end-to-end here so
/// the stamping wiring can't silently regress.
#[test]
fn with_actor_stamps_local_for_init_and_handler() {
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
    impl HandlesKind<Tick> for LocalProbe {
        type Sender = aether_actor::Anyone;
    }

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
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            // Init runs inside the chassis builder's stamp guard
            // — write a sentinel so the handler test below proves
            // the same slots are reused across init→dispatch.
            Counter::with_mut(|c| c.0 = 100);
            Ok(Self { observed: params })
        }
    }

    impl aether_actor::Declared for LocalProbe {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
    }
    impl NativeActor for LocalProbe {
        type State = Self;
    }

    impl Dispatch<Self> for LocalProbe {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Anyone, crate::Unchecked>,
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

    // Three dispatches. Init seeded 100; the handler bumps once
    // per dispatch and snapshots — so observed should walk
    // 101, 102, 103 in order. Each root settles once its handler
    // has run.
    let probe = chassis.actor_ref::<LocalProbe>();
    let roots: Vec<_> = (0..3).map(|seq| chassis.send_tracked(probe, &Tick { seq }, None).1).collect();
    for settled in &roots {
        await_settled(settled, "test.local.tick");
    }
    assert_eq!(
        observed.load(AtomicOrdering::SeqCst),
        103,
        "init seeded 100 + 3 handler bumps ⇒ Local at 103 (proves the same \
         ActorSlots is stamped across init and dispatch)"
    );

    drop(chassis);
}

/// A two-cap fixture for the composed-reference record: each cap counts the
/// `Ping`s it receives into its own counter.
macro_rules! counting_cap {
    ($type:ident, $namespace:literal, $ping:ty) => {
        struct $type {
            received: Arc<AtomicU32>,
        }
        impl Addressable for $type {
            const NAMESPACE: &'static str = $namespace;
            type Resolver = aether_actor::One;
        }
        impl aether_actor::Root for $type {}
        impl HandlesKind<$ping> for $type {
            type Sender = aether_actor::Anyone;
        }
        impl aether_actor::Lifecycle<Self> for $type {
            type Config = ();
            type Params = Arc<AtomicU32>;
            type InitError = BootError;
            type InitCtx<'a> = NativeInitCtx<'a>;
            type Ctx<'a> = NativeCtx<'a, Self>;
            fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { received: params })
            }
        }
        impl aether_actor::Declared for $type {
            type Depends = ();
            type Spawns = ();
            type Parents = ();
        }
        impl NativeActor for $type {
            type State = Self;
        }
        impl Dispatch<Self> for $type {
            fn dispatch(
                state: &mut Self,
                _ctx: &mut NativeCtx<'_, Self, crate::Anyone, crate::Unchecked>,
                kind: KindId,
                _payload: &[u8],
            ) -> Option<()> {
                if kind != <$ping as aether_data::Kind>::ID {
                    return None;
                }
                state.received.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(())
            }
        }
    };
}

pod_kind!(ComposedPing { tag: u32 }, "test.composed.ping", 0xA1B2_C3D4_E5F6_0003);
counting_cap!(ComposedLeft, "test.composed.left", ComposedPing);
counting_cap!(ComposedRight, "test.composed.right", ComposedPing);

/// ADR-0230: the chassis records one proof per composed singleton, keyed by
/// type, once its route is `Live`. A record keyed by the wrong `TypeId`, or
/// written before the slot is live, hands out a proof that reaches the wrong
/// actor or nothing — so each cap must receive exactly the mail sent through
/// its own `actor_ref`.
#[test]
fn actor_ref_reaches_each_composed_cap_and_only_it() {
    use std::sync::atomic::Ordering as AtomicOrdering;

    let (registry, mailer) = bare_substrate();
    let left = Arc::new(AtomicU32::new(0));
    let right = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<ComposedLeft>(Arc::clone(&left))
        .with_actor::<ComposedRight>(Arc::clone(&right))
        .build_passive()
        .expect("both caps boot");

    let ping = ComposedPing { tag: 7 };
    let left_ref = chassis.actor_ref::<ComposedLeft>();
    let right_ref = chassis.actor_ref::<ComposedRight>();
    let (_, left_settled) = chassis.send_tracked(left_ref, &ping, None);
    let (_, right_settled) = chassis.send_tracked(right_ref, &ping, None);
    let (_, right_again) = chassis.send_tracked(right_ref, &ping, None);
    await_settled(&left_settled, "test.composed.left");
    await_settled(&right_settled, "test.composed.right");
    await_settled(&right_again, "test.composed.right_again");
    assert_eq!(left.load(AtomicOrdering::SeqCst), 1, "the left cap receives only the mail sent to its reference");
    assert_eq!(right.load(AtomicOrdering::SeqCst), 2, "the right cap receives only the mail sent to its reference");

    drop(chassis);
}

/// Asking for a reference the chassis never composed is a wiring bug, and the
/// panic names the missing actor so the caller can find it.
#[test]
#[should_panic(expected = "test.composed.right")]
fn actor_ref_panics_naming_an_uncomposed_actor() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<ComposedLeft>(Arc::new(AtomicU32::new(0)))
        .build_passive()
        .expect("the left cap boots");

    let _ = chassis.actor_ref::<ComposedRight>();
}
