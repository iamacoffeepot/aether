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
