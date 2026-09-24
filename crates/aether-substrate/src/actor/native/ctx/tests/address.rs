//! Typed receiver resolution off a ctx: an embedded peer folds from the
//! binding's logical parent (ADR-0099 §3), and the proof verbs hand back
//! proven references.

use std::sync::Arc;

use aether_actor::{Addressable, ErasedActorRef};
use aether_data::{Kind, MailboxId, mailbox_id_from_path};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;
use crate::mail::{Source, SourceAddr};

use super::support::{CastOnly, EmbeddedPeer};

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
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, Some(parent)));
    assert_eq!(binding.parent_mailbox(), Some(parent));

    {
        let ctx = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);
        let peer = ctx.actor::<EmbeddedPeer>();
        assert_eq!(peer.mailbox_id(), recipient);
        peer.send(&CastOnly { code: 17 });
    }

    let delivered = rx.try_recv().expect("embedded peer send routes at ctx flush");
    assert_eq!(delivered.kind, CastOnly::ID);
}

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
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, Some(parent)));
    let ctx: NativeCtx<'_, Dependent, Single> =
        NativeCtx::new_for_actor(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);

    assert_eq!(ctx.actor_ref::<OneDep>().id(), ctx.actor::<OneDep>().mailbox_id());
    assert_eq!(ctx.actor_ref::<EmbeddedPeer>().id(), ctx.actor::<EmbeddedPeer>().mailbox_id());
}

/// `sender` mints the stamped dispatch source on the erased ctx: `Some` for
/// a `SourceAddr::Component` source, proving exactly its id, and `None` for
/// `SourceAddr::None`. Owned logic: the source classification `sender`
/// performs itself.
#[test]
fn sender_mints_the_component_source_and_none_without_one() {
    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let parent = MailboxId(0xC020);
    let current = MailboxId(0xC010);
    let binding = Arc::new(NativeBinding::new_for_test_with_parent(Arc::clone(&mailer), current, Some(parent)));

    let component =
        NativeCtx::new(&binding, Source::with_correlation(SourceAddr::Component(MailboxId(0xC030)), 0), None, None);
    assert_eq!(
        component.sender().map(ErasedActorRef::id),
        Some(MailboxId(0xC030)),
        "a component source mints a sender reference to its id"
    );

    let sourceless = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);
    assert!(sourceless.sender().is_none(), "a sourceless dispatch has no sender reference");
}
