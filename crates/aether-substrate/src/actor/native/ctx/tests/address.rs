//! Typed receiver resolution off a ctx: a declared root dependency folds
//! from the root whatever the actor's lineage (ADR-0099 §3), and the proof
//! verbs hand back proven references.

use std::sync::Arc;

use aether_actor::{ActorRef, Addressable, HandlesKind};

use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, Subname, TaskDone};
use crate::chassis::error::BootError;
use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::testing::{PumpedDriver, bare_substrate, boot_bare_test_chassis, registered_ref};

use super::support::{CastOnly, ReaderRig, sink};

/// Asks a [`Nest`] to stage its [`Dependent`] child.
#[aether_data::kind(name = "test.native.actor_ref_hatch")]
struct Hatch;

/// Asks a [`Dependent`] to send through the proof `actor_ref` mints.
#[aether_data::kind(name = "test.native.actor_ref_fold")]
struct Fold;

/// A pumped root that parents one [`Dependent`], and keeps the proof its
/// birth's completion hands back.
#[derive(Default)]
struct Nest {
    child: Option<ActorRef<Dependent>>,
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for Nest {
    const NAMESPACE: &'static str = "test.native.actor_ref_nest";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::single]
    fn on_hatch(&mut self, ctx: &mut NativeCtx<'_>, _hatch: Hatch) {
        let _ = self;
        let _receipt =
            ctx.spawn_child::<Dependent>(Subname::Named("dependent"), (), ()).stage().expect("the birth stages");
    }

    #[handler(task)]
    fn on_born(&mut self, _ctx: &mut NativeCtx<'_>, done: TaskDone<SpawnOutcome<Dependent>>) {
        self.child = done.into_output().result.ok();
    }
}

/// A child placed beneath [`Nest`], so its own lineage is not the root's,
/// that declares the root singleton [`OneDep`].
struct Dependent;

#[aether_actor::actor(instanced, child_of(Nest), depends(OneDep))]
impl NativeActor for Dependent {
    const NAMESPACE: &'static str = "test.native.actor_ref_dependent";
    type Config = ();

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_fold(&mut self, ctx: &mut NativeCtx<'_>, _fold: Fold) {
        let _ = self;
        ctx.send_to(ctx.actor_ref::<OneDep>(), &CastOnly { code: 1 });
    }
}

struct OneDep;

impl Addressable for OneDep {
    const NAMESPACE: &'static str = "test.native.actor_ref_one_dep";
    type Resolver = aether_actor::One;
}

impl HandlesKind<CastOnly> for OneDep {}

/// `actor_ref` on a child's ctx proves the root position a `One` dependency
/// folds to, not a position beneath the child's own lineage, with no registry
/// read: the send through the proof lands on the inbox standing at the root
/// fold. Owned logic: the scope selection `actor_ref` feeds the resolver.
#[test]
fn actor_ref_mints_the_root_fold_for_a_one_dependency() {
    let (registry, mailer) = bare_substrate();
    let (_one_dep, landed) = sink(&registry, &mailer, OneDep::NAMESPACE);
    let mut driver = PumpedDriver::<Nest>::boot(boot_bare_test_chassis(&registry, &mailer), (), ());
    let nest = driver.chassis().actor_ref::<Nest>();

    driver.send_and_settle(nest, &Hatch, None);
    let child = driver.read_state(|nest| nest.child).flatten().expect("the dependent is born beneath the nest");
    driver.send_and_settle(child, &Fold, None);

    let folded = landed.try_recv().expect("the send through the minted proof lands at the root fold");
    assert_eq!(folded.recipient, OneDep::resolve(0, ()));
}

/// `sender` mints the stamped dispatch source only when it holds a route in
/// this substrate's registry: `Some` of the very actor whose real send the
/// turn handles, and `None` for a sourceless chassis push. Owned logic: the
/// source classification and the route read `sender` performs itself.
#[test]
fn sender_mints_only_a_routed_component_source() {
    let mut rig = ReaderRig::boot();
    let pinger = rig.pinger("routed");

    rig.ping(pinger);
    rig.knock(None);

    let senders = rig.senders();
    assert_eq!(
        senders[0].as_ref().map(|(sender, _path)| *sender),
        Some(pinger.erase()),
        "a routed sender mints its own reference"
    );
    assert!(senders[1].is_none(), "a sourceless dispatch has no sender reference");
}

/// `resolve_path` proves a path whose route is `Live` to the very reference
/// registered there, answers `NotLive` naming the canonical path for a route
/// whose birth is still `Starting`, and hands back the registry's own refusal
/// as `Unresolved` for a path that resolves to no route. Owned logic: the
/// verb's pairing of address resolution with the liveness proof, and the
/// split between its two refusals, which the component host's reply text
/// reads.
#[test]
fn resolve_path_proves_live_routes_and_names_the_rest() {
    use aether_data::ErasedActorPath;

    use crate::actor::native::ResolvePathError;

    let mut rig = ReaderRig::boot();
    let live = registered_ref(&rig.registry, "test.native.resolve_path_live", discharging());
    let starting = "test.native.resolve_path_starting";
    rig.registry.reserve_starting_through_owner(starting).expect("owner accepts the Starting reservation");
    let path = |text: &str| ErasedActorPath::new(text).expect("a valid actor path");

    let (live_proof, starting_proof, unknown_proof) = rig
        .driver
        .host_turn(|_reader, ctx| {
            (
                ctx.resolve_path(&path("test.native.resolve_path_live")),
                ctx.resolve_path(&path(starting)),
                ctx.resolve_path(&path("test.native.resolve_path_unknown")),
            )
        })
        .expect("the reader is live");

    assert_eq!(live_proof, Ok(live));
    assert_eq!(
        starting_proof,
        Err(ResolvePathError::NotLive { canonical_path: starting.to_owned() }),
        "a Starting route resolves as an address but does not prove",
    );
    assert!(
        matches!(unknown_proof, Err(ResolvePathError::Unresolved(_))),
        "a path with no route is the registry's own refusal"
    );
}

fn discharging() -> Arc<dyn InboxHandler> {
    Arc::new(|dispatch: OwnedDispatch| dispatch.discharge())
}
