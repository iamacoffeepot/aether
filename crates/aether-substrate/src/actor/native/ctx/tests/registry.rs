//! Registry reads off a ctx: a proven reference answers its actor's
//! canonical path, before and after that actor departs, and so does the
//! sender reference a real dispatch stamps, which also answers where its
//! actor stands by creation order.

use std::sync::Arc;

use aether_actor::Addressable;
use aether_data::ErasedActorPath;

use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::testing::{drop_ref, registered_ref};

use super::support::{Ping, Pinger, ReaderRig};

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

/// The renderer orders draws by the lineage order of the sender each dispatch
/// stamped. Two pingers knock in the reverse of the order they were created;
/// the order read for each stamped sender is its own actor's, so they compare
/// by creation. It catches a ctx read wired to the wrong reference, such as
/// the reading actor's own, which would answer one order for every sender.
#[test]
fn lineage_order_ranks_stamped_senders_by_creation() {
    let mut rig = ReaderRig::boot();
    let (first, second) = (rig.pinger("first"), rig.pinger("second"));

    rig.ping(second);
    rig.ping(first);

    let stamped: Vec<_> =
        rig.senders().into_iter().map(|sender| sender.expect("a routed send carries its sender").0).collect();
    assert_eq!(stamped, [second.erase(), first.erase()], "the knocks arrived in the reverse of creation order");
    let orders = rig
        .driver
        .host_turn(move |_reader, ctx| stamped.iter().map(|sender| ctx.lineage_order(*sender)).collect::<Vec<_>>())
        .expect("the reader is live");
    assert!(orders[1] < orders[0], "the first-created pinger sorts before the second");
}
