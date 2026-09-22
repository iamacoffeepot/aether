//! Typed receiver resolution off a ctx: an embedded peer folds from the
//! binding's logical parent, and a keyed peer folds from whichever scope its
//! resolver declares — with the caller's in-flight lineage retained either
//! way (ADR-0099 §3 / ADR-0080 §7).

use std::sync::Arc;

use aether_actor::Addressable;
use aether_data::{Kind, MailId, MailboxId, mailbox_id_from_path};

use crate::actor::native::NativeCtx;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::mail::{Source, SourceAddr};

use super::support::{CastOnly, CurrentKeyedPeer, EmbeddedPeer, ParentKeyedPeer};

/// A typed embedded recipient resolves from the binding's logical parent,
/// and a send through that handle reaches the mailbox registered beneath
/// that parent. The parent is already a tagged routable `MailboxId`; no raw
/// carry is retained beside it.
#[allow(clippy::disallowed_methods)] // test scaffolding — synthetic lineage IDs exercise parent-relative routing
#[test]
fn embedded_actor_resolves_and_delivers_beneath_binding_parent() {
    use crate::mail::registry::OwnedDispatch;
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let parent = mailbox_id_from_path("test.native.parent");
    let current = mailbox_id_from_path("test.native.parent/test.native.caller");
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
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, parent));
    assert_eq!(binding.parent_mailbox(), parent);

    {
        let ctx = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), MailId::NONE, MailId::NONE);
        let peer = ctx.actor::<EmbeddedPeer>();
        assert_eq!(peer.mailbox_id(), recipient);
        peer.send(&CastOnly { code: 17 });
    }

    let delivered = rx.try_recv().expect("embedded peer send routes at ctx flush");
    assert_eq!(delivered.kind, CastOnly::ID);
}

/// Keyed typed construction selects the recipient resolver's declared
/// scope: built-in `Many` folds from the calling actor, while a test-only
/// keyed resolver can fold from its logical parent. Both returned handles
/// retain the binding and in-flight causal context, proven by delivery
/// under the handled mail's root and parent edge.
#[allow(clippy::disallowed_methods)] // test scaffolding — synthetic lineage IDs exercise scoped routing
#[test]
fn keyed_actor_resolution_selects_scope_and_retains_native_context() {
    use crate::mail::registry::OwnedDispatch;
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let parent = mailbox_id_from_path("test.native.keyed_parent");
    let current = mailbox_id_from_path("test.native.keyed_parent/test.native.keyed_caller");
    let current_target = CurrentKeyedPeer::resolve(current.0, "current");
    let parent_target = ParentKeyedPeer::resolve(parent.0, "parent");
    let (current_tx, current_rx) = mpsc::channel::<Envelope>();
    let (parent_tx, parent_rx) = mpsc::channel::<Envelope>();

    registry
        .try_register_inbox_with_id(
            &boot_authority(),
            current_target,
            "test.native.keyed_parent/test.native.keyed_caller/test.native.current_keyed_peer:current",
            Arc::new(move |dispatch: OwnedDispatch| {
                dispatch.discharge();
                let _ = current_tx.send(dispatch);
            }),
        )
        .expect("register current-scoped keyed peer");
    registry
        .try_register_inbox_with_id(
            &boot_authority(),
            parent_target,
            "test.native.keyed_parent/test.native.parent_keyed_peer:parent",
            Arc::new(move |dispatch: OwnedDispatch| {
                dispatch.discharge();
                let _ = parent_tx.send(dispatch);
            }),
        )
        .expect("register parent-scoped keyed peer");

    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, parent));
    let in_flight_root = MailId::new(MailboxId(0xC0), 7);
    let in_flight_mail = MailId::new(MailboxId(0x99), 42);
    let source = Source::with_correlation(SourceAddr::None, 0);

    {
        let ctx = NativeCtx::new(&binding, source, in_flight_mail, in_flight_root);
        let current_peer = ctx.resolve_actor::<CurrentKeyedPeer>("current");
        let parent_peer = ctx.resolve_actor::<ParentKeyedPeer>("parent");

        assert_eq!(current_peer.mailbox_id(), current_target, "Many selects the current actor's mailbox");
        assert_eq!(parent_peer.mailbox_id(), parent_target, "the custom keyed resolver selects the logical parent");

        current_peer.send(&CastOnly { code: 21 });
        parent_peer.send(&CastOnly { code: 22 });
    }

    let current_delivered = current_rx.try_recv().expect("current-scoped keyed send routes at ctx flush");
    let parent_delivered = parent_rx.try_recv().expect("parent-scoped keyed send routes at ctx flush");
    for delivered in [current_delivered, parent_delivered] {
        assert_eq!(delivered.kind, CastOnly::ID);
        assert_eq!(delivered.root, in_flight_root, "resolved handle retains the caller's root");
        assert_eq!(delivered.parent_mail, Some(in_flight_mail), "resolved handle retains the handled mail parent");
    }
}

/// `ctx.resolve` tracks a keyed child across a genuine spawn: `None` before
/// the spawn machinery publishes it, `Some` after — and the reference proves
/// the spawned child's position. The eager terminal commits synchronously,
/// so the test reads both sides of the transition without a running pool.
#[test]
fn resolve_tracks_a_keyed_child_across_spawn() {
    use aether_actor::{Lifecycle, Manual, address_at};
    use aether_data::LoadName;

    use crate::actor::native::identity::ActorRuntimeIdentity;
    use crate::actor::native::spawn::{SpawnBuilder, Spawner, Subname};
    use crate::actor::native::{Dispatch, NativeActor, NativeInitCtx};
    use crate::actor::registry::ActorRegistry;
    use crate::chassis::error::BootError;
    use crate::config::RingCapacities;
    use crate::mail::KindId;
    use crate::runtime::lifecycle::PanicAborter;
    use crate::scheduler::WakeSink;
    use crate::testing::bare_substrate;

    struct ResolveChild;

    impl Addressable for ResolveChild {
        const NAMESPACE: &'static str = "test.native.resolve_child";
        type Resolver = aether_actor::Many;
    }

    impl Lifecycle<Self> for ResolveChild {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }

    impl Dispatch<Self> for ResolveChild {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    impl NativeActor for ResolveChild {
        type State = Self;
    }

    let (registry, mailer) = bare_substrate();
    let current = MailboxId(0x4010);
    let grandparent = MailboxId(0x4020);
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, grandparent));
    let ctx = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), MailId::NONE, MailId::NONE);
    let address = address_at::<ResolveChild>(LoadName::new("child-a").expect("a valid test subname"));
    assert!(ctx.resolve(&address).is_none(), "an unspawned child resolves to no reference");

    let spawner = Arc::new(Spawner::new(
        Arc::clone(&registry),
        Arc::new(ActorRegistry::new()),
        Arc::clone(&mailer),
        Arc::new(PanicAborter),
        WakeSink::detached(),
        RingCapacities::default(),
    ));
    let parent = ActorRuntimeIdentity::new(current, grandparent, current.0, Arc::from("test.native.resolve_parent"));
    let child_id =
        SpawnBuilder::<'_, ResolveChild>::new_child(spawner, Subname::Named("child-a"), (), (), Source::NONE, parent)
            .finish()
            .expect("spawn the keyed child");

    let resolved = ctx.resolve(&address).expect("a spawned child resolves to a reference");
    assert_eq!(resolved.id(), child_id, "the reference proves the spawned child's position");
}

struct Dependent;

impl Addressable for Dependent {
    const NAMESPACE: &'static str = "test.native.actor_ref_dependent";
    type Resolver = aether_actor::One;
}

struct OneDep;

impl Addressable for OneDep {
    const NAMESPACE: &'static str = "test.native.actor_ref_one_dep";
    type Resolver = aether_actor::One;
}

impl aether_actor::DependsOn<OneDep> for Dependent {}
impl aether_actor::DependsOn<EmbeddedPeer> for Dependent {}

/// `actor_ref` and `actor` share one derivation on a `NativeCtx<'_, Dependent>`:
/// the reference proves the folded position for a `One` and for an `Embedded`
/// dependency, with no registry read. Owned logic: the shared `actor` fold
/// behind both doors.
#[test]
fn actor_ref_mints_the_position_actor_folds_for_one_and_embedded_dependencies() {
    use aether_actor::Single;

    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let parent = MailboxId(0xC020);
    let current = MailboxId(0xC010);
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, parent));
    let ctx: NativeCtx<'_, Dependent, Single> =
        NativeCtx::new_for_actor(&binding, Source::with_correlation(SourceAddr::None, 0), MailId::NONE, MailId::NONE);

    assert_eq!(ctx.actor_ref::<OneDep>().id(), ctx.actor::<OneDep>().mailbox_id());
    assert_eq!(ctx.actor_ref::<EmbeddedPeer>().id(), ctx.actor::<EmbeddedPeer>().mailbox_id());
}

/// `me` mints the binding's own mailbox as a typed reference on a
/// `NativeCtx<'_, Dependent>`: the id it proves equals `self_id()`. Owned
/// logic: the birth-bound mint, which performs no registry read.
#[test]
fn me_mints_the_binding_mailbox_as_a_typed_reference() {
    use aether_actor::Single;

    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let parent = MailboxId(0xC020);
    let current = MailboxId(0xC010);
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, parent));
    let ctx: NativeCtx<'_, Dependent, Single> =
        NativeCtx::new_for_actor(&binding, Source::with_correlation(SourceAddr::None, 0), MailId::NONE, MailId::NONE);

    assert_eq!(ctx.me().id(), ctx.self_id());
}

/// `sender` mints the stamped dispatch source on the erased ctx: `Some` for
/// a `SourceAddr::Component` source, `None` for `SourceAddr::None`. Owned
/// logic: the `source_mailbox` lift, which adds no source-classification of
/// its own.
#[test]
fn sender_mints_the_component_source_and_none_without_one() {
    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let parent = MailboxId(0xC020);
    let current = MailboxId(0xC010);
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, parent));

    let component = NativeCtx::new(
        &binding,
        Source::with_correlation(SourceAddr::Component(MailboxId(0xC030)), 0),
        MailId::NONE,
        MailId::NONE,
    );
    assert!(component.sender().is_some(), "a component source mints a sender reference");

    let sourceless =
        NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), MailId::NONE, MailId::NONE);
    assert!(sourceless.sender().is_none(), "a sourceless dispatch has no sender reference");
}
