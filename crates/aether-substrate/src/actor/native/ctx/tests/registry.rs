//! Registry reads off a ctx: a proven reference answers its actor's
//! canonical path, before and after that actor departs, and so does the
//! sender reference a real dispatch stamps.

use std::sync::Arc;
use std::sync::mpsc;

use aether_data::ActorPath;

use crate::actor::native::NativeCtx;
use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::mail::{Source, SourceAddr};
use crate::testing::{bare_substrate, drop_ref, registered_binding, registered_ref};

use super::support::CastOnly;

fn discharging() -> Arc<dyn InboxHandler> {
    Arc::new(|dispatch: OwnedDispatch| dispatch.discharge())
}

/// The path is read from a route of any lifecycle, so a peer that has since
/// departed is still named: the dropped-mailbox case is the one a log line
/// most needs a name for, and a lookup that read only `Live` routes would
/// blank it.
#[test]
fn actor_path_names_a_peer_before_and_after_it_departs() {
    let (registry, mailer) = bare_substrate();
    let (binding, _caller) = registered_binding(&registry, &mailer, "test.native.path_host", discharging());
    let peer_name = "test.native.path_peer";
    let peer = registered_ref(&registry, peer_name, discharging());
    let expected = ActorPath::new(peer_name).expect("the peer name is a canonical path");
    let ctx = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);

    assert_eq!(ctx.actor_path(peer), expected);

    drop_ref(&registry, peer);

    assert_eq!(ctx.actor_path(peer), expected);
}

/// `actor_path` panics on a reference with no route record, so every
/// reference `sender` hands a handler must name one. A real send stamps the
/// sending actor's position on the envelope; the receiving ctx's `sender`
/// proves exactly that actor, and its path answers the sender's name — while
/// the sender is live and after it departs.
#[test]
fn actor_path_names_the_sender_a_real_dispatch_stamps() {
    let (registry, mailer) = bare_substrate();
    let sender_name = "test.native.stamp_sender";
    let (sender_binding, sender) = registered_binding(&registry, &mailer, sender_name, discharging());
    let (tx, rx) = mpsc::channel::<OwnedDispatch>();
    let (receiver_binding, receiver) = registered_binding(
        &registry,
        &mailer,
        "test.native.stamp_receiver",
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let expected = ActorPath::new(sender_name).expect("the sender name is a canonical path");

    {
        let mut ctx = NativeCtx::new(&sender_binding, Source::with_correlation(SourceAddr::None, 0), None, None);
        ctx.send_to(receiver, &CastOnly { code: 3 });
    }
    let delivered = rx.try_recv().expect("the send routes at ctx flush");
    let ctx = NativeCtx::new(&receiver_binding, delivered.sender, delivered.mail_id, delivered.root);
    let stamped = ctx.sender().expect("a routed actor's send carries its sender");

    assert_eq!(stamped, sender);
    assert_eq!(ctx.actor_path(stamped), expected);

    drop_ref(&registry, sender);

    assert_eq!(ctx.sender(), Some(sender), "a departed sender still holds its route record");
    assert_eq!(ctx.actor_path(stamped), expected);
}
