//! ADR-0165 seal fixtures. The `Spawner` holds the last `BootAuthority` in
//! circulation, so "was the seal installed" is exactly "is that token gone".
//! A driver's Claim hook is the earliest place a test can reach the live
//! `Arc<Spawner>`, and it is the only reach that survives a build whose driver
//! `Start` fails — the `BootedPassives` that would otherwise hold it is
//! dropped on that path.

use super::support::DrivenTestChassis;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::{Builder, DriverCapability, DriverCtx, DriverRunning, RunError};
use crate::chassis::ctx::ChassisCtx;
use crate::mail::KindId;
use crate::mail::Mail;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use std::cell::RefCell;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering as AtomicOrdering;

thread_local! {
    static PROBED_SPAWNER: RefCell<Option<Arc<crate::Spawner>>> = const { RefCell::new(None) };
}

/// Driver that stashes the chassis `Spawner` at Claim, then either boots
/// or fails its Start stage.
struct SealProbeDriver {
    fail_start: bool,
}

struct SealProbeRunning;

impl DriverCapability for SealProbeDriver {
    type Running = SealProbeRunning;

    fn claim(ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        PROBED_SPAWNER.with(|slot| slot.borrow_mut().replace(Arc::clone(ctx.spawner_arc())));
        Ok(())
    }

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        if self.fail_start {
            return Err(BootError::Other(Box::new(io::Error::other("driver Start refused"))));
        }
        Ok(SealProbeRunning)
    }
}

impl DriverRunning for SealProbeRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        Ok(())
    }
}

fn take_probed_spawner() -> Arc<crate::Spawner> {
    PROBED_SPAWNER.with(|slot| slot.borrow_mut().take()).expect("the driver Claim hook stashed the spawner")
}

/// The actor the post-seal external spawn tests below bring up. Inert by
/// design: what is under test is the birth protocol around it, not
/// anything it does once live.
struct Spawned;

impl Addressable for Spawned {
    const NAMESPACE: &'static str = "test.chassis_builder.seal.post_seal_spawn";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for Spawned {}
impl aether_actor::Lifecycle<Self> for Spawned {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init((): Self::Config, (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }
}
impl NativeActor for Spawned {
    type State = Self;
}
impl Dispatch<Self> for Spawned {
    fn dispatch(
        _state: &mut Self,
        _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
        _kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        None
    }
}

/// A passive chassis seals immediately before it is handed to the
/// embedder, and everything the embedder spawns afterwards still lands —
/// through the owner, because the token that reaches the direct writer no
/// longer exists.
///
/// Tripwire: the seal must be *installed* (the token is gone) and must not
/// cost the embedder its spawn surface. Dropping either half — a seal that
/// never runs, or one that strands `spawn_actor` — fails here. The
/// `registry.entry` assertion is the read-your-writes half: the birth
/// completion the caller blocks on has to fire after the owner published
/// the Live route, because `entry` answers `None` for a `Starting` one.
#[test]
fn passive_chassis_seals_before_it_is_returned_and_still_spawns() {
    let (registry, mailer) = bare_substrate();
    let chassis =
        Builder::<TestChassis>::new(Arc::clone(&registry), mailer).build_passive().expect("empty chassis boots");

    assert!(
        chassis.booted.spawner.seal().is_none(),
        "build_passive seals before returning, so no boot authority survives into the embedder"
    );

    let id = chassis
        .spawn_actor::<Spawned>(crate::Subname::Named("post-seal"), (), ())
        .finish()
        .expect("a post-seal root birth still lands");
    assert!(registry.entry(id).is_some(), "the owner-routed root birth reached Live before finish() returned");
}

/// A post-seal external birth the owner refuses comes back as a typed
/// [`crate::SpawnError`] on the calling thread.
///
/// Tripwire: the birth completion has two arms and only the promotion one
/// is obvious. Wire the external channel to `promote` alone and a refused
/// birth has nobody left to answer it — this call then sits out its whole
/// patience budget and reports `BirthWedged` instead of the name that is
/// actually in the way, which is what the assertion below reads.
#[test]
fn post_seal_spawn_of_a_live_subname_returns_a_typed_error() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(registry, mailer).build_passive().expect("empty chassis boots");

    chassis
        .spawn_actor::<Spawned>(crate::Subname::Named("taken"), (), ())
        .finish()
        .expect("the first post-seal birth lands");

    let refused = chassis
        .spawn_actor::<Spawned>(crate::Subname::Named("taken"), (), ())
        .finish()
        .expect_err("a second birth on a live name is refused");

    assert!(
        matches!(&refused, crate::SpawnError::SubnameInUse { full_name } if full_name.contains("taken")),
        "the owner's refusal reaches the caller's thread intact: {refused:?}",
    );
}

pod_kind!(Poke { value: u32 }, "test.chassis_builder.seal.poke", 0x5ea1_0001);

/// A pumped actor booted onto an already-sealed passive chassis runs the
/// two-ack handshake against the registry owner: reserve `Starting`, run
/// `init` / `wire` on this thread (its execution home), then hand the
/// owner the wired endpoint to publish.
///
/// Tripwire: the route the owner publishes has to be the endpoint the
/// caller thread wired, not a fresh one — asserting on liveness alone
/// would pass against a route bound to a dead handler, so the check is
/// that mail addressed to the actor arrives at the slot the caller holds.
#[test]
fn post_seal_pumped_boot_publishes_the_endpoint_the_caller_wired() {
    struct Pumped {
        seen: Arc<AtomicU32>,
    }
    impl Addressable for Pumped {
        const NAMESPACE: &'static str = "test.chassis_builder.seal.pumped";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for Pumped {}
    impl HandlesKind<Poke> for Pumped {}
    impl aether_actor::Lifecycle<Self> for Pumped {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): Self::Config, params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { seen: params })
        }
    }
    impl NativeActor for Pumped {
        type State = Self;
    }
    impl Dispatch<Self> for Pumped {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == <Poke as aether_data::Kind>::ID.0 {
                let poke = <Poke as aether_data::Kind>::decode_from_bytes(payload)?;
                state.seen.fetch_add(poke.value, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let seen = Arc::new(AtomicU32::new(0));
    let (mut slot, _wake) =
        chassis.boot_pumped_actor::<Pumped>((), Arc::clone(&seen)).expect("post-seal pumped boot succeeds");

    let id = registry.lookup(Pumped::NAMESPACE).expect("the owner published the pumped route");
    assert!(registry.entry(id).is_some(), "the second ack promoted the reservation to Live");

    mailer.push(Mail::new(
        id,
        <Poke as aether_data::Kind>::ID,
        <Poke as aether_data::Kind>::encode_into_bytes(&Poke { value: 7 }),
        1,
    ));
    slot.drain_available();
    assert_eq!(seen.load(AtomicOrdering::SeqCst), 7, "mail routed to the endpoint the caller thread wired");
}

/// A built chassis seals after its driver's `Start` returns `Ok`.
#[test]
fn built_chassis_seals_after_a_successful_driver_start() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<DrivenTestChassis<SealProbeDriver>>::new(registry, mailer)
        .driver(SealProbeDriver { fail_start: false })
        .build()
        .expect("build succeeds");

    assert!(
        take_probed_spawner().seal().is_none(),
        "a successful driver Start is followed by the seal, so the boot authority is spent"
    );
    drop(chassis);
}

/// A driver whose `Start` fails must leave boot's own writer intact: the
/// chassis is unwinding, and the teardown that follows is still boot.
///
/// Tripwire: moving the seal ahead of `driver_boot(..)?` — the obvious
/// "seal once the passives are up" simplification — is exactly what this
/// catches.
#[test]
fn failed_driver_start_does_not_seal() {
    let (registry, mailer) = bare_substrate();
    let error = Builder::<DrivenTestChassis<SealProbeDriver>>::new(registry, mailer)
        .driver(SealProbeDriver { fail_start: true })
        .build()
        .expect_err("the driver refuses its Start stage");
    drop(error);

    assert!(
        take_probed_spawner().seal().is_some(),
        "a failed driver Start never reaches the seal, so boot's authority is still held"
    );
}
