//! Registry reads off a ctx: a proven reference answers its actor's
//! canonical path, before and after that actor departs, and so does the
//! sender reference a real dispatch stamps. A registry batch staged from a
//! held reply answers the caller that reply captured.

use std::sync::Arc;
use std::sync::mpsc;

use aether_data::{ErasedActorPath, Kind};

use crate::actor::native::NativeCtx;
use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::mail::{Source, SourceAddr};
use crate::testing::{bare_substrate, drop_ref, registered_binding, registered_ref};

use super::support::{CastOnly, HeldRig, LedgerRead, StageReq, TestReply};

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
    let expected = ErasedActorPath::new(peer_name).expect("the peer name is a canonical path");
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
    let expected = ErasedActorPath::new(sender_name).expect("the sender name is a canonical path");

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

/// Catches `stage_registry_batch_from` arming its completion from the staging
/// ctx instead of the owed value: that would take a second settlement hold on
/// the caller's chain, which the completion's resolve never releases. The
/// batch's completion must own the one hold the `Held` took and answer the
/// caller the `Held` captured, from the later completion turn.
#[test]
fn a_batch_staged_from_a_held_reply_answers_the_held_caller() {
    let mut rig = HeldRig::boot();
    let (caller, replies) = rig.caller("test.native.batch_from.caller");

    let root = rig.push(&StageReq, Some(caller));
    rig.driver.settle(&[root]);

    let (staged, batches) = rig.driver.read_state(|host| (host.staged, host.batches)).expect("the host is live");
    assert_eq!(
        staged,
        Some(LedgerRead { entry: None, held_open: 1 }),
        "staging moved the held entry into the batch's, which owns the one hold the Held took, not a second",
    );
    assert_eq!(batches, 1, "the batch's completion ran on a later turn");

    let reply = replies.try_recv().expect("the answer reaches the held caller");
    assert_eq!(reply.sender.correlation_id, 77, "the held correlation is echoed, not the completion turn's");
    assert_eq!(TestReply::decode_from_bytes(reply.payload.bytes()), Some(TestReply { value: 5 }));
    assert_eq!(rig.held_open(root), 0, "resolving releases the one hold");
}
