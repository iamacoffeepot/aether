//! The `wire` pass: wire-time mail crosses actors in either declaration order,
//! and `wire` runs exactly once — at chassis boot for a singleton, and on a
//! runtime spawn for an instanced actor.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Issue 697 multi-pass model: wire-time mail crosses actors
/// regardless of declaration order. Pinger's `wire` mails Ponger;
/// Ponger's handler increments a counter. With Pinger declared
/// FIRST, a single-pass interleaved boot would have Pinger's wire
/// fire before Ponger's claim — the mail would warn-drop. The
/// multi-pass model (claim-all → init-all → wire-all → spawn-all)
/// claims both mailboxes before any wire runs, so the mail queues
/// in Ponger's inbox and processes once dispatchers come up.
#[test]
fn wire_pass_mail_crosses_actors_pinger_first() {
    wire_pass_mail_crosses_actors(/* pinger_first */ true);
}

/// Mirror of [`wire_pass_mail_crosses_actors_pinger_first`] with
/// the registration order reversed. Multi-pass model means both
/// orderings are valid; this test pins the symmetry.
#[test]
fn wire_pass_mail_crosses_actors_ponger_first() {
    wire_pass_mail_crosses_actors(/* pinger_first */ false);
}

/// Issue 584 Phase 2a runtime sibling: `Spawner::spawn_actor` runs
/// `wire` exactly once on a freshly-spawned instanced actor —
/// after `init` Ok and after the mailbox is published, before
/// pre-load mail or the dispatcher pull. Runtime spawn doesn't
/// need the chassis-boot multi-pass barrier (the substrate is
/// already steady-state).
#[test]
fn spawn_actor_runs_wire_once_after_init() {
    use crate::actor::native::spawn::Subname;

    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    struct WireSpawnProbe {
        wire_count: Arc<AtomicU32>,
    }
    impl Addressable for WireSpawnProbe {
        const NAMESPACE: &'static str = "test.spawn_wire.probe";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for WireSpawnProbe {}
    impl aether_actor::Lifecycle<Self> for WireSpawnProbe {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_count: params })
        }
        fn wire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            state.wire_count.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl NativeActor for WireSpawnProbe {
        type State = Self;
    }
    impl Dispatch<Self> for WireSpawnProbe {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let wire_count = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let id = chassis
        .spawn_actor::<WireSpawnProbe>(Subname::Counter, (), Arc::clone(&wire_count))
        .finish()
        .expect("spawn instanced actor");

    assert_eq!(wire_count.load(AtomicOrdering::SeqCst), 1, "wire must fire exactly once on Spawner::spawn_actor");

    drop(chassis);
    let _ = id;
}

/// Issue 584 Phase 2a / 697 wire pass: `wire` runs exactly once
/// for a singleton actor at chassis boot, after `init` succeeds
/// and before the dispatcher pulls the first envelope.
#[test]
fn with_actor_runs_wire_once_at_chassis_boot() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    struct WireProbe {
        wire_count: Arc<AtomicU32>,
    }
    impl Addressable for WireProbe {
        const NAMESPACE: &'static str = "test.wire.singleton";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for WireProbe {}
    impl aether_actor::Lifecycle<Self> for WireProbe {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_count: params })
        }
        fn wire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            state.wire_count.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl NativeActor for WireProbe {
        type State = Self;
    }
    impl Dispatch<Self> for WireProbe {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let wire_count = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<WireProbe>(Arc::clone(&wire_count))
        .build_passive()
        .expect("with_actor boot succeeds");

    assert_eq!(
        wire_count.load(AtomicOrdering::SeqCst),
        1,
        "wire must fire exactly once during builder.with_actor boot",
    );

    drop(chassis);
}

fn wire_pass_mail_crosses_actors(pinger_first: bool) {
    use aether_actor::MailSender;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(WireBarrierPing { tag: u32 }, "test.barrier.wire_ping", 0xB0B1_B2B3_B4B5_B6B7);

    struct Pinger {
        wire_ran: Arc<AtomicU32>,
    }
    impl Addressable for Pinger {
        const NAMESPACE: &'static str = "test.barrier.pinger";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for Pinger {}
    impl aether_actor::Lifecycle<Self> for Pinger {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_ran: params })
        }
        fn wire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
            ctx.send_to_named::<WireBarrierPing>(Ponger::NAMESPACE, &WireBarrierPing { tag: 1 });
            state.wire_ran.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl NativeActor for Pinger {
        type State = Self;
    }
    impl Dispatch<Self> for Pinger {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    struct Ponger {
        received: Arc<AtomicU32>,
    }
    impl Addressable for Ponger {
        const NAMESPACE: &'static str = "test.barrier.ponger";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for Ponger {}
    impl HandlesKind<WireBarrierPing> for Ponger {}
    impl aether_actor::Lifecycle<Self> for Ponger {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received: params })
        }
    }
    impl NativeActor for Ponger {
        type State = Self;
    }
    impl Dispatch<Self> for Ponger {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WireBarrierPing::ID.0 {
                let _ = WireBarrierPing::decode_from_bytes(payload)?;
                state.received.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let received = Arc::new(AtomicU32::new(0));
    let wire_ran = Arc::new(AtomicU32::new(0));

    let builder = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer));
    let builder = if pinger_first {
        builder.with_actor::<Pinger>(Arc::clone(&wire_ran)).with_actor::<Ponger>(Arc::clone(&received))
    } else {
        builder.with_actor::<Ponger>(Arc::clone(&received)).with_actor::<Pinger>(Arc::clone(&wire_ran))
    };
    let chassis = builder.build_passive().expect("multi-pass boot succeeds");

    assert_eq!(wire_ran.load(AtomicOrdering::SeqCst), 1, "pinger's wire must have run during the wire pass");

    // Wait for Ponger's dispatcher to drain the wire-emitted ping.
    let deadline = Instant::now() + Duration::from_millis(500);
    while received.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        received.load(AtomicOrdering::SeqCst),
        1,
        "ponger must observe pinger's wire-emitted ping (multi-pass barrier)",
    );

    drop(chassis);
}
