//! Native declared dependencies checked at birth (ADR-0230): every
//! declared dependency is checked, and a `#[actor(depends(R))]` actor
//! whose dependency is not `Live` is refused
//! before `init` at every birth site — the passive boot, the spawner, and
//! the pumped-slot boot — naming both actors. A dependency on a pumped slot
//! reserved at the Claim stage passes, and the build fails if that slot is
//! never booted.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aether_actor::Addressable;

use crate::actor::native::SpawnError;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::spawn::Subname;
use crate::chassis::builder::Builder;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};

pod_kind!(Probe { tag: u32 }, "test.deps.probe", 0xDE90_0001_0000_0001);

struct AudioDep;

#[aether_actor::actor(root)]
impl NativeActor for AudioDep {
    const NAMESPACE: &'static str = "test.deps.audio";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::single]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let _ = self;
    }
}

struct VideoDep;

#[aether_actor::actor(root)]
impl NativeActor for VideoDep {
    const NAMESPACE: &'static str = "test.deps.video";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::single]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let _ = self;
    }
}

struct PairDependent;

#[aether_actor::actor(root, depends(AudioDep, VideoDep))]
impl NativeActor for PairDependent {
    const NAMESPACE: &'static str = "test.deps.pair";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::single]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let _ = self;
    }
}

static LONELY_INIT_RAN: AtomicBool = AtomicBool::new(false);

struct LonelyDependent;

#[aether_actor::actor(root, depends(AudioDep))]
impl NativeActor for LonelyDependent {
    const NAMESPACE: &'static str = "test.deps.lonely";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        LONELY_INIT_RAN.store(true, Ordering::SeqCst);
        Ok(Self)
    }

    #[aether_actor::handler::single]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let _ = self;
    }
}

struct OrderedDependent;

#[aether_actor::actor(root, depends(AudioDep))]
impl NativeActor for OrderedDependent {
    const NAMESPACE: &'static str = "test.deps.ordered";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::single]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let _ = self;
    }
}

struct SpawnedDependent;

#[aether_actor::actor(instanced, root, depends(AudioDep))]
impl NativeActor for SpawnedDependent {
    const NAMESPACE: &'static str = "test.deps.spawned";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::single]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let _ = self;
    }
}

struct PumpedDependent;

#[aether_actor::actor(root, depends(AudioDep))]
impl NativeActor for PumpedDependent {
    const NAMESPACE: &'static str = "test.deps.pumped";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::single]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let _ = self;
    }
}

/// A dependent composed without its dependency fails the build naming both
/// actors, before its `init` runs, and the failed build leaves nothing
/// claimed.
#[test]
fn missing_declared_dependency_fails_build_before_init() {
    let (registry, mailer) = bare_substrate();

    let err = Builder::<TestChassis>::new(Arc::clone(&registry), mailer)
        .with_actor::<LonelyDependent>(())
        .build_passive()
        .expect_err("a dependent without its dependency must fail the build");

    assert_eq!(err.to_string(), "test.deps.lonely depends on test.deps.audio, which is not live");
    let BootError::DependencyNotLive { actor, namespace } = err else {
        panic!("unexpected error: {err:?}");
    };
    assert_eq!(actor, LonelyDependent::NAMESPACE);
    assert_eq!(namespace, AudioDep::NAMESPACE);
    assert!(!LONELY_INIT_RAN.load(Ordering::SeqCst), "refused before init ran");
    assert_eq!(registry.lookup(LonelyDependent::NAMESPACE), None, "a failed build leaves nothing claimed");
}

/// An actor with two declared dependencies is refused when either one is
/// missing, whichever order the inventory lists its declarations in.
#[test]
fn each_declared_dependency_is_checked() {
    let (registry, mailer) = bare_substrate();

    let err = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<AudioDep>(())
        .with_actor::<PairDependent>(())
        .build_passive()
        .expect_err("a dependent missing its video dependency must fail the build");

    let BootError::DependencyNotLive { actor, namespace } = err else {
        panic!("unexpected error: {err:?}");
    };
    assert_eq!(actor, PairDependent::NAMESPACE);
    assert_eq!(namespace, VideoDep::NAMESPACE);

    let (registry, mailer) = bare_substrate();

    let err = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<VideoDep>(())
        .with_actor::<PairDependent>(())
        .build_passive()
        .expect_err("a dependent missing its audio dependency must fail the build");

    let BootError::DependencyNotLive { actor, namespace } = err else {
        panic!("unexpected error: {err:?}");
    };
    assert_eq!(actor, PairDependent::NAMESPACE);
    assert_eq!(namespace, AudioDep::NAMESPACE);
}

/// Composition order does not matter: the dependent declared FIRST still
/// builds when its dependency is part of the chassis.
#[test]
fn dependent_declared_first_builds_with_its_dependency() {
    let (registry, mailer) = bare_substrate();

    let _chassis = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<OrderedDependent>(())
        .with_actor::<AudioDep>(())
        .build_passive()
        .expect("the dependent declared first still builds when its dependency is composed");
}

/// A spawned child whose declared dependency is not `Live` fails its spawn
/// as a failed `init` does.
#[test]
fn spawned_child_with_missing_dependency_fails_spawn() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(registry, mailer).build_passive().expect("empty chassis boots");

    let err = chassis
        .spawn_actor::<SpawnedDependent>(Subname::Counter, (), ())
        .finish()
        .expect_err("a spawned child with a missing dependency must fail");

    let SpawnError::InitFailed(BootError::DependencyNotLive { actor, namespace }) = err else {
        panic!("unexpected spawn error: {err:?}");
    };
    assert_eq!(actor, SpawnedDependent::NAMESPACE);
    assert_eq!(namespace, AudioDep::NAMESPACE);
}

/// A pumped actor booted through `PassiveChassis::boot_pumped_actor` with a
/// missing declared dependency is refused naming both actors.
#[test]
fn pumped_actor_with_missing_dependency_fails_boot() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(registry, mailer).build_passive().expect("empty chassis boots");

    let Err(err) = chassis.boot_pumped_actor::<PumpedDependent>((), ()) else {
        panic!("a pumped actor with a missing dependency must fail");
    };

    let BootError::DependencyNotLive { actor, namespace } = err else {
        panic!("unexpected pumped boot error: {err:?}");
    };
    assert_eq!(actor, PumpedDependent::NAMESPACE);
    assert_eq!(namespace, AudioDep::NAMESPACE);
}

/// A passive that depends on a pumped actor builds when the chassis reserves
/// the pumped slot at the Claim stage and boots it in the build's start: the
/// reservation is live before the passive's `init`, and the start recovers it
/// without re-claiming the name.
#[test]
fn passive_dependent_boots_on_a_reserved_pumped_slot() {
    let (registry, mailer) = bare_substrate();

    let (passive, (mut slot, _wake)) = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<OrderedDependent>(())
        .reserve_pumped::<AudioDep>()
        .build_passive_with_start(|passive| passive.boot_pumped_actor::<AudioDep>((), ()))
        .expect("a passive depending on a reserved pumped slot builds once the start boots the slot");

    let _audio = passive.actor_ref::<AudioDep>();
    slot.shutdown();
}

/// A pumped slot reserved at the Claim stage and never booted fails the
/// build naming the slot, through the start terminal and through the plain
/// passive terminal alike.
#[test]
fn reserved_pumped_slot_never_booted_fails_the_build() {
    let (registry, mailer) = bare_substrate();
    let err = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<OrderedDependent>(())
        .reserve_pumped::<AudioDep>()
        .build_passive_with_start(|_| Ok(()))
        .expect_err("a start that never boots the reserved slot must fail the build");
    assert!(err.to_string().contains("\"test.deps.audio\""), "the error names the unbooted slot: {err}");

    let (registry, mailer) = bare_substrate();
    let err = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<OrderedDependent>(())
        .reserve_pumped::<AudioDep>()
        .build_passive()
        .expect_err("the plain passive terminal boots nothing, so a reservation must fail it");
    assert!(err.to_string().contains("\"test.deps.audio\""), "the error names the unbooted slot: {err}");
}
