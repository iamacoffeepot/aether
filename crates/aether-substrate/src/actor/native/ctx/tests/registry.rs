//! Registry reads off a ctx: a proven reference answers its actor's
//! canonical path, before and after that actor departs, and so does the
//! sender reference a real dispatch stamps. A registry batch staged from a
//! held reply answers the caller that reply captured.

use std::sync::Arc;

use aether_actor::Addressable;
use aether_data::{ErasedActorPath, Kind};

use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::testing::{drop_ref, registered_ref};

use super::support::{HeldRig, LedgerRead, Ping, Pinger, ReaderRig, StageReq, TestReply};

fn discharging() -> Arc<dyn InboxHandler> {
    Arc::new(|dispatch: OwnedDispatch| dispatch.discharge())
}

/// The path is read from a route of any lifecycle, so a peer that has since
/// departed is still named: the dropped-mailbox case is the one a log line
/// most needs a name for, and a lookup that read only `Live` routes would
/// blank it.
#[test]
fn actor_path_names_a_peer_before_and_after_it_departs() {
    let mut rig = ReaderRig::boot();
    let peer_name = "test.native.path_peer";
    let peer = registered_ref(&rig.registry, peer_name, discharging());
    let expected = ErasedActorPath::new(peer_name).expect("the peer name is a canonical path");
    let path = |rig: &mut ReaderRig| rig.driver.host_turn(|_reader, ctx| ctx.actor_path(peer));

    assert_eq!(path(&mut rig), Some(expected.clone()));

    drop_ref(&rig.registry, peer);

    assert_eq!(path(&mut rig), Some(expected));
}

/// `actor_path` panics on a reference with no route record, so every
/// reference `sender` hands a handler must name one. A real send stamps the
/// sending actor's position on the envelope; the receiving turn's `sender`
/// proves exactly that actor, and its path answers the sender's name — while
/// the sender is live, and on a knock it sent that the reader handles only
/// after the sender has departed.
#[test]
fn actor_path_names_the_sender_a_real_dispatch_stamps() {
    let mut rig = ReaderRig::boot();
    let pinger = rig.pinger("stamp");
    let expected = ErasedActorPath::new(&format!("{}:stamp", Pinger::NAMESPACE)).expect("a canonical instance path");

    rig.ping(pinger);

    let stamped = rig.senders()[0].clone().expect("a routed actor's send carries its sender");
    assert_eq!(stamped, (pinger.erase(), expected.clone()), "the turn proves the sending actor and names it");

    // The pinger handles `Ping` before `Leave`, so its knock waits in the
    // pumped reader's inbox while the pinger closes; settling the ping's
    // chain then runs the reader's turn after its sender departed.
    let knock = rig.driver.send_tracked(pinger, &Ping, None);
    rig.close(pinger);
    rig.driver.settle(&[knock]);

    assert_eq!(
        rig.senders()[1],
        Some((pinger.erase(), expected)),
        "a departed sender still holds its route record, so a knock it sent before departing still proves it",
    );
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
