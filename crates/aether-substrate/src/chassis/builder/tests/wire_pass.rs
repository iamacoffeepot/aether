//! The `wire` pass: wire-time mail crosses actors in either declaration order,
//! and `wire` runs exactly once — at chassis boot for a singleton, and on a
//! runtime spawn for an instanced actor.

use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::spawn::Subname;
use crate::actor::native::{Dispatch, SpawnOutcome, TaskDone};
use crate::chassis::builder::{Builder, PassiveChassis};
use crate::mail::KindId;
use crate::runtime::trace::SettlementHold;
use crate::testing::{TestChassis, await_settled, await_signal, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use aether_data::{ErasedActorPath, Kind, LoadName};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

pod_kind!(WireBarrierPing { tag: u32 }, "test.barrier.wire_ping", 0xB0B1_B2B3_B4B5_B6B7);

/// Counts every `WireBarrierPing` it handles — the composed peer each
/// wire-sending probe below mails.
struct Ponger {
    received: Arc<AtomicU32>,
}
impl Addressable for Ponger {
    const NAMESPACE: &'static str = "test.barrier.ponger";
    type Resolver = aether_actor::One;
}
impl aether_actor::Root for Ponger {}
impl HandlesKind<WireBarrierPing> for Ponger {
    type Sender = aether_actor::Anyone;
}
impl aether_actor::Lifecycle<Self> for Ponger {
    type Config = ();
    type Params = Arc<AtomicU32>;
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { received: params })
    }
}
impl aether_actor::Declared for Ponger {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for Ponger {
    type State = Self;
}
impl Dispatch<Self> for Ponger {
    fn dispatch(
        state: &mut Self,
        _ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
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

/// An embedder-spawned actor whose `wire` mails the composed [`Ponger`].
struct SpawnedPinger;

#[aether_actor::actor(instanced, root, depends(Ponger))]
impl NativeActor for SpawnedPinger {
    const NAMESPACE: &'static str = "test.spawn_wire.pinger";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
        ctx.send::<Ponger>(&WireBarrierPing { tag: 2 });
        Ok(())
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut NativeCtx<'_>, _env: &Envelope) {
        let _ = self;
    }
}

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
///
/// Its `wire` sends nothing, so `finish_wire_settled` returns only because the
/// spawn's wire root settles on its hold's release alone.
#[test]
fn spawn_actor_runs_wire_once_after_init() {
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
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_count: params })
        }
        fn wire(state: &mut Self, _ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
            state.wire_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }
    impl aether_actor::Declared for WireSpawnProbe {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
    }
    impl NativeActor for WireSpawnProbe {
        type State = Self;
    }
    impl Dispatch<Self> for WireSpawnProbe {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
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
        .finish_wire_settled()
        .expect("spawn instanced actor");

    assert_eq!(wire_count.load(AtomicOrdering::SeqCst), 1, "wire must fire exactly once on Spawner::spawn_actor");

    drop(chassis);
    let _ = id;
}

/// ADR-0244: an embedder spawn's `wire` sends settle under the spawn's own
/// wire root, so once `finish_wire_settled` returns the composed peer has
/// handled the mail — no poll. A send that minted its own root, or a hold
/// released before the held mail was flushed, would let it return first.
#[test]
fn spawn_wire_mail_settles_under_the_spawn_root() {
    let (registry, mailer) = bare_substrate();
    let received = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<Ponger>(Arc::clone(&received))
        .build_passive()
        .expect("ponger boots");

    chassis
        .spawn_actor::<SpawnedPinger>(Subname::Counter, (), ())
        .finish_wire_settled()
        .expect("spawn the wire-sending pinger");

    assert_eq!(received.load(AtomicOrdering::SeqCst), 1, "ponger must have handled the spawned pinger's wire ping");

    drop(chassis);
}

/// Asks a [`StagingParent`] to stage its [`WiredChild`].
#[aether_data::kind(name = "test.staged_wire.stage")]
struct StageWired;

/// Tells a [`WiredChild`] to release the hold its `wire` took.
#[aether_data::kind(name = "test.staged_wire.release")]
struct ReleaseWireHold;

/// The key [`StagingParent`] stages its [`WiredChild`] under, and the path
/// the child is born at.
const WIRED_KEY: &str = "wired";
const WIRED_PATH: &str = "test.staged_wire.parent/test.staged_wire.child:wired";

/// A root whose handler stages one [`WiredChild`], the handler-staged birth
/// every guest load also is, and signals once the birth's completion is back.
struct StagingParent {
    born: Sender<()>,
}

#[aether_actor::actor(root)]
impl NativeActor for StagingParent {
    const NAMESPACE: &'static str = "test.staged_wire.parent";
    type Config = ();
    type Params = Sender<()>;

    fn init((): (), born: Sender<()>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { born })
    }

    #[handler::tell]
    fn on_stage(&mut self, ctx: &mut NativeCtx<'_>, _stage: StageWired) {
        let _ = self;
        let _receipt =
            ctx.spawn_child::<WiredChild>(Subname::Named(WIRED_KEY), (), ()).stage().expect("the birth stages");
    }

    #[handler(task)]
    fn on_born(&mut self, _ctx: &mut NativeCtx<'_>, done: TaskDone<SpawnOutcome<WiredChild>>) {
        assert!(done.into_output().result.is_ok(), "the wired child is born");
        let _ = self.born.send(());
    }
}

/// A handler-staged child whose `wire` mails the composed [`Ponger`] and
/// takes a hold, which it keeps until told to release it.
struct WiredChild {
    hold: Option<SettlementHold>,
}

#[aether_actor::actor(instanced, child_of(StagingParent), depends(Ponger))]
impl NativeActor for WiredChild {
    const NAMESPACE: &'static str = "test.staged_wire.child";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { hold: None })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
        self.hold = ctx.acquire_settlement_hold();
        ctx.send::<Ponger>(&WireBarrierPing { tag: 3 });
        Ok(())
    }

    #[handler::tell]
    fn on_release(&mut self, _ctx: &mut NativeCtx<'_>, _release: ReleaseWireHold) {
        self.hold = None;
    }
}

/// Boot a [`Ponger`] and a [`StagingParent`], have the parent's handler
/// stage its [`WiredChild`], wait for the birth's completion and then on the
/// child's `wire`, and answer the chassis, the ponger's count, and the
/// receiver for the staging chain's settlement.
fn stage_wired_child() -> (PassiveChassis<TestChassis>, Arc<AtomicU32>, Receiver<()>) {
    let (registry, mailer) = bare_substrate();
    let received = Arc::new(AtomicU32::new(0));
    let (born_tx, born) = crossbeam_channel::bounded(1);
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<Ponger>(Arc::clone(&received))
        .with_actor::<StagingParent>(born_tx)
        .build_passive()
        .expect("ponger and staging parent boot");

    let (_, staged) = chassis.send_tracked(chassis.actor_ref::<StagingParent>(), &StageWired, None);
    await_signal(&born, "test.staged_wire.born");
    chassis.await_wire_settled(&ErasedActorPath::new(WIRED_PATH).expect("the wired child's path is well formed"));

    (chassis, received, staged)
}

/// ADR-0244 §2: a handler-staged birth's `wire` sends settle under the
/// birth's own wire root, so once `await_wire_settled` returns the composed
/// peer has handled the mail, with no poll. A `wire` send that minted its own
/// root, or a hold released before the held mail was flushed, would let the
/// wait return first.
#[test]
fn handler_staged_wire_mail_settles_under_the_birth_root() {
    let (chassis, received, _staged) = stage_wired_child();

    assert_eq!(received.load(AtomicOrdering::SeqCst), 1, "ponger must have handled the staged child's wire ping");

    drop(chassis);
}

/// ADR-0244 §2 and ADR-0168 §1: a hold a handler-staged birth's `wire` takes
/// gates the chain that caused the birth, never the birth's wire root. The
/// wire root settles while the hold is still held, and the staging chain
/// settles only once the hold is released. A hold that gated the wire root
/// would wedge `await_wire_settled`, and one that gated nothing would let the
/// staging chain settle early.
#[test]
fn handler_staged_wire_hold_gates_the_causing_chain() {
    let (chassis, _received, staged) = stage_wired_child();

    assert!(
        matches!(staged.try_recv(), Err(TryRecvError::Empty)),
        "the hold the staged child's wire took must keep the staging chain open"
    );

    let parent = chassis.actor_ref::<StagingParent>();
    let child = chassis
        .child::<StagingParent, WiredChild>(parent, LoadName::new(WIRED_KEY).expect("a valid key"))
        .expect("the wired child is live");
    let (_, released) = chassis.send_tracked(child, &ReleaseWireHold, None);
    await_settled(&released, "test.staged_wire.release");
    await_settled(&staged, "test.staged_wire.staging_chain");

    drop(chassis);
}

/// Issue 584 Phase 2a / 697 wire pass: `wire` runs exactly once
/// for a singleton actor at chassis boot, after `init` succeeds
/// and before the dispatcher pulls the first envelope.
#[test]
fn with_actor_runs_wire_once_at_chassis_boot() {
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
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_count: params })
        }
        fn wire(state: &mut Self, _ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
            state.wire_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }
    impl aether_actor::Declared for WireProbe {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
    }
    impl NativeActor for WireProbe {
        type State = Self;
    }
    impl Dispatch<Self> for WireProbe {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
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

/// Catches a boot rollback that drops a wired root: when a later passive
/// fails after the wire pass, the earlier root, whose `wire` ran, is closed,
/// so its `unwire` runs exactly once (ADR-0247 rule 5).
///
/// The failure is a stand-in passive whose own `wire` refuses, driven through
/// the real `boot_passives` beside a real root boot, so the root under test is
/// one whose own `wire` succeeded.
#[test]
fn a_boot_that_fails_after_the_wire_pass_closes_the_roots_that_wired() {
    use super::super::boot_passives::{BootTuning, boot_passives};
    use super::super::native_actor_boot::NativeActorBoot;
    use super::super::passive_boot::{DynShutdown, PassiveBoot};
    use crate::chassis::ctx::ChassisCtx;
    use crate::config::{ConfigSources, RegistryQueueCapacities, RingCapacities, SchedulerTuning};
    use crate::mail::MailId;
    use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
    use std::io;
    use std::time::Duration;

    struct WiredRoot {
        wired: Arc<AtomicU32>,
        unwired: Arc<AtomicU32>,
    }

    #[aether_actor::actor(root)]
    impl NativeActor for WiredRoot {
        const NAMESPACE: &'static str = "test.rollback.wired_root";
        type Config = ();
        type Params = (Arc<AtomicU32>, Arc<AtomicU32>);

        fn init(
            (): (),
            params: (Arc<AtomicU32>, Arc<AtomicU32>),
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            let (wired, unwired) = params;
            Ok(Self { wired, unwired })
        }

        fn wire(&mut self, _ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
            self.wired.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }

        fn unwire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            state.unwired.fetch_add(1, AtomicOrdering::SeqCst);
        }

        #[fallback]
        fn fallback(&mut self, _ctx: &mut NativeCtx<'_>, _env: &Envelope) {
            let _ = self;
        }
    }

    struct RefusesAtWire;

    impl PassiveBoot for RefusesAtWire {
        fn claim(&mut self, _ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
            Ok(())
        }

        fn wire(&mut self, _wire_root: Option<MailId>) -> Result<(), BootError> {
            Err(BootError::Other(Box::new(io::Error::other("this passive's wire refused"))))
        }

        fn spawn(self: Box<Self>, _ctx: &mut ChassisCtx<'_>) -> Result<Box<dyn DynShutdown>, BootError> {
            unreachable!("a passive whose wire refused is never spawned")
        }

        fn cleanup_after_failure(self: Box<Self>, _ctx: &mut ChassisCtx<'_>) {}
    }

    let (registry, mailer) = bare_substrate();
    let wired = Arc::new(AtomicU32::new(0));
    let unwired = Arc::new(AtomicU32::new(0));
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let passives: Vec<Box<dyn PassiveBoot>> = vec![
        Box::new(NativeActorBoot::<WiredRoot>::new((Arc::clone(&wired), Arc::clone(&unwired)))),
        Box::new(RefusesAtWire),
    ];

    let booted = boot_passives(
        &registry,
        &mailer,
        &aborter,
        BootTuning {
            workers: Some(2),
            ring_capacities: RingCapacities::default(),
            scheduler_tuning: SchedulerTuning::default(),
            registry_queues: RegistryQueueCapacities::default(),
            teardown_budget: Duration::from_mins(1),
        },
        &mut ConfigSources::default(),
        passives,
        |_ctx| Ok(()),
    );

    assert!(booted.is_err(), "the boot fails on the passive whose wire refused");
    assert_eq!(wired.load(AtomicOrdering::SeqCst), 1, "the root wired before the later passive failed");
    assert_eq!(unwired.load(AtomicOrdering::SeqCst), 1, "the rollback closed the wired root, so its unwire ran");
}

/// A composed capability whose `wire` returns an error fails the chassis
/// build with that error (ADR-0247 rule 3). Catches the wire pass swallowing
/// the hook's result (the build would succeed with a half-wired root), and a
/// failed root rolled back as if it had never wired (its `unwire` would not
/// run, nor would the `unwire` of the sibling that wired before it).
#[test]
fn a_composed_capability_whose_wire_fails_fails_the_build() {
    use std::io;

    const REFUSAL: &str = "this capability's wire refused";

    struct WiresFirst {
        unwired: Arc<AtomicU32>,
    }

    #[aether_actor::actor(root)]
    impl NativeActor for WiresFirst {
        const NAMESPACE: &'static str = "test.wire_failure.wires_first";
        type Config = ();
        type Params = Arc<AtomicU32>;

        fn init((): (), unwired: Arc<AtomicU32>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { unwired })
        }

        fn unwire(&mut self, _ctx: &mut NativeCtx<'_, Self>) {
            self.unwired.fetch_add(1, AtomicOrdering::SeqCst);
        }

        #[fallback]
        fn fallback(&mut self, _ctx: &mut NativeCtx<'_>, _env: &Envelope) {
            let _ = self;
        }
    }

    struct RefusesWire {
        unwired: Arc<AtomicU32>,
    }

    #[aether_actor::actor(root)]
    impl NativeActor for RefusesWire {
        const NAMESPACE: &'static str = "test.wire_failure.refuses";
        type Config = ();
        type Params = Arc<AtomicU32>;

        fn init((): (), unwired: Arc<AtomicU32>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { unwired })
        }

        fn wire(&mut self, _ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
            let _ = self;
            Err(BootError::Other(Box::new(io::Error::other(REFUSAL))))
        }

        fn unwire(&mut self, _ctx: &mut NativeCtx<'_, Self>) {
            self.unwired.fetch_add(1, AtomicOrdering::SeqCst);
        }

        #[fallback]
        fn fallback(&mut self, _ctx: &mut NativeCtx<'_>, _env: &Envelope) {
            let _ = self;
        }
    }

    let (registry, mailer) = bare_substrate();
    let sibling_unwired = Arc::new(AtomicU32::new(0));
    let refuser_unwired = Arc::new(AtomicU32::new(0));

    let built = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<WiresFirst>(Arc::clone(&sibling_unwired))
        .with_actor::<RefusesWire>(Arc::clone(&refuser_unwired))
        .build_passive();

    let Err(error) = built else {
        panic!("a capability whose wire returned an error must fail the build");
    };
    assert!(error.to_string().contains(REFUSAL), "the build error is the hook's own: {error}");
    assert_eq!(refuser_unwired.load(AtomicOrdering::SeqCst), 1, "the root that entered wire ran its unwire");
    assert_eq!(sibling_unwired.load(AtomicOrdering::SeqCst), 1, "the root that wired before it ran its unwire");
}

fn wire_pass_mail_crosses_actors(pinger_first: bool) {
    struct Pinger {
        wire_ran: Arc<AtomicU32>,
    }

    #[aether_actor::actor(root, depends(Ponger))]
    impl NativeActor for Pinger {
        const NAMESPACE: &'static str = "test.barrier.pinger";
        type Config = ();
        type Params = Arc<AtomicU32>;

        fn init((): (), params: Arc<AtomicU32>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_ran: params })
        }

        fn wire(&mut self, ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
            ctx.send::<Ponger>(&WireBarrierPing { tag: 1 });
            self.wire_ran.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }

        #[fallback]
        fn fallback(&mut self, _ctx: &mut NativeCtx<'_>, _env: &Envelope) {
            let _ = self;
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

    // ADR-0244: the wire pass ran under the boot's wire root, which the seal
    // released, so its settlement means Ponger has handled the ping.
    chassis.await_boot_settled();
    assert_eq!(
        received.load(AtomicOrdering::SeqCst),
        1,
        "ponger must observe pinger's wire-emitted ping (multi-pass barrier)",
    );

    drop(chassis);
}
