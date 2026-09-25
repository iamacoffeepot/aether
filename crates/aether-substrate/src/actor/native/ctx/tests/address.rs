//! Typed receiver resolution off a ctx: an embedded peer folds from the
//! binding's logical parent (ADR-0099 §3), and the proof verbs hand back
//! proven references.

use std::sync::Arc;

use aether_actor::Addressable;
use aether_data::{Kind, MailboxId};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;
use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::mail::{Source, SourceAddr};

use super::support::{CastOnly, EmbeddedPeer};

struct Dependent;

#[aether_actor::actor(depends(OneDep, EmbeddedPeer))]
impl NativeActor for Dependent {
    const NAMESPACE: &'static str = "test.native.actor_ref_dependent";
    type Config = ();

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut NativeCtx<'_>, _env: &Envelope) {
        let _ = self;
    }
}

struct OneDep;

impl Addressable for OneDep {
    const NAMESPACE: &'static str = "test.native.actor_ref_one_dep";
    type Resolver = aether_actor::One;
}

/// A typed embedded recipient resolves from the binding's logical parent,
/// and a flat send to that declared dependency reaches the mailbox registered
/// beneath that parent. The parent is already a tagged routable `MailboxId`;
/// no raw carry is retained beside it.
#[test]
fn embedded_actor_resolves_and_delivers_beneath_binding_parent() {
    use crate::mail::registry::lineage_mailbox_id;
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let parent = lineage_mailbox_id("test.native.parent");
    let current = lineage_mailbox_id("test.native.parent/test.native.caller");
    let recipient = EmbeddedPeer::resolve(parent.0, ());
    let (tx, rx) = mpsc::channel::<Envelope>();
    registry
        .try_register_inbox_with_id(
            &boot_authority(),
            recipient,
            "test.native.parent/test.embedded_peer",
            Arc::new(move |dispatch: OwnedDispatch| {
                dispatch.discharge();
                let _ = tx.send(dispatch);
            }),
        )
        .expect("register embedded peer beneath the runtime parent");
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, Some(parent)));
    assert_eq!(binding.parent_mailbox(), Some(parent));

    {
        let mut ctx: NativeCtx<'_, Dependent> =
            NativeCtx::new_for_actor(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);
        ctx.send::<EmbeddedPeer>(&CastOnly { code: 17 });
    }

    let delivered = rx.try_recv().expect("embedded peer send routes at ctx flush");
    assert_eq!(delivered.kind, CastOnly::ID);
}

/// `actor_ref` on a `NativeCtx<'_, Dependent>` proves the position each
/// dependency's resolver folds beneath the binding's scope, for a `One` and
/// for an `Embedded` dependency, with no registry read. Owned logic: the
/// scope selection `actor_ref` feeds the resolver.
#[test]
fn actor_ref_mints_the_resolver_fold_for_one_and_embedded_dependencies() {
    use aether_actor::Single;

    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let parent = MailboxId(0xC020);
    let current = MailboxId(0xC010);
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, Some(parent)));
    let ctx: NativeCtx<'_, Dependent, Single> =
        NativeCtx::new_for_actor(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);

    assert_eq!(ctx.actor_ref::<OneDep>().id(), OneDep::resolve(current.0, ()));
    assert_eq!(ctx.actor_ref::<EmbeddedPeer>().id(), EmbeddedPeer::resolve(parent.0, ()));
}

/// `sender` mints the stamped dispatch source only when it holds a route in
/// this substrate's registry: `Some` of the very reference registered here,
/// `None` for a component source routed only in another registry, and `None`
/// for `SourceAddr::None`. Owned logic: the source classification and the
/// route read `sender` performs itself.
#[test]
fn sender_mints_only_a_routed_component_source() {
    use crate::testing::{bare_substrate, registered_binding, registered_ref};

    let (registry, mailer) = bare_substrate();
    let (binding, _receiver) = registered_binding(&registry, &mailer, "test.native.sender_host", discharging());
    let routed = registered_ref(&registry, "test.native.sender_routed", discharging());
    let (elsewhere_registry, _elsewhere_mailer) = bare_substrate();
    let unrouted = registered_ref(&elsewhere_registry, "test.native.sender_elsewhere", discharging());

    let component =
        NativeCtx::new(&binding, Source::with_correlation(SourceAddr::Component(routed.id()), 0), None, None);
    assert_eq!(component.sender(), Some(routed), "a routed component source mints its own reference");

    let forged =
        NativeCtx::new(&binding, Source::with_correlation(SourceAddr::Component(unrouted.id()), 0), None, None);
    assert!(forged.sender().is_none(), "a component source with no route here mints nothing");

    let sourceless = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);
    assert!(sourceless.sender().is_none(), "a sourceless dispatch has no sender reference");
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
    use aether_data::ActorPath;

    use crate::actor::native::ResolvePathError;
    use crate::config::RegistryQueueCapacities;
    use crate::mail::registry::RegistryOwnerLease;
    use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
    use crate::scheduler::{Pool, PoolConfig};
    use crate::testing::{bare_substrate, boot_authority, registered_binding, registered_ref};

    let (registry, mailer) = bare_substrate();
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, aborter);
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        pool.wake_sink(),
        RegistryQueueCapacities::default(),
    );
    let (binding, _host) = registered_binding(&registry, &mailer, "test.native.resolve_path_host", discharging());
    let live = registered_ref(&registry, "test.native.resolve_path_live", discharging());
    let starting = "test.native.resolve_path_starting";
    registry.reserve_starting_through_owner(starting).expect("owner accepts the Starting reservation");
    let ctx = NativeCtx::new(&binding, Source::NONE, None, None);
    let path = |text: &str| ActorPath::new(text).expect("a valid actor path");

    assert_eq!(ctx.resolve_path(&path("test.native.resolve_path_live")), Ok(live));
    assert_eq!(
        ctx.resolve_path(&path(starting)),
        Err(ResolvePathError::NotLive { canonical_path: starting.to_owned() }),
        "a Starting route resolves as an address but does not prove",
    );
    assert!(
        matches!(ctx.resolve_path(&path("test.native.resolve_path_unknown")), Err(ResolvePathError::Unresolved(_))),
        "a path with no route is the registry's own refusal",
    );

    drop(ctx);
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

fn discharging() -> Arc<dyn InboxHandler> {
    Arc::new(|dispatch: OwnedDispatch| dispatch.discharge())
}
