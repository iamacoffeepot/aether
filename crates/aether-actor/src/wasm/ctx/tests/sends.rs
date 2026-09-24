//! The reply-mode-free send view (`ctx.sends()`): that it routes and stamps
//! outbound mail exactly as the [`WasmCtx`] it was taken from.

use super::{NO_INBOUND_SOURCE, Registry, WasmCtx, recording_target};
use crate::model::ctx::{Erased, Manual};
use crate::model::{Addressable, Embedded, HandlesKind};
use crate::reference::{ActorRef, ErasedActorRef};
use crate::wasm::inline::drain_cluster_queue;
use alloc::string::String;
use alloc::vec::Vec;

struct SendsPeer;

impl Addressable for SendsPeer {
    const NAMESPACE: &'static str = "test.wasm.sends_peer";
    type Resolver = Embedded;
}

impl HandlesKind<()> for SendsPeer {}

/// Tripwire: `Sends` carries its own copy of the routing every `WasmCtx` send
/// verb performs, so the two can drift. A view that stamped a different `from`,
/// routed to a different recipient, or bypassed the inline registry would show
/// up here as a missing dispatch or a different observed source.
///
/// Every recipient here is a cluster member, so each send routes in place and
/// enqueues locally — no host call. A `()` payload encodes to empty bytes.
/// Synthetic ids keep the fixture off the name fold.
#[test]
fn sends_view_routes_and_stamps_like_the_ctx_it_came_from() {
    use aether_data::MailboxId;

    let registry = Registry::new();
    let root = MailboxId(0x7400);
    registry.set_self_id(root.0);

    let target_id = MailboxId(0x7401);
    let probe = recording_target();
    registry.insert_child(target_id, 0, String::from("test.wasm.sends_child"), false, root.0, Vec::new(), probe.actor);
    let target = ErasedActorRef::new(target_id);

    let mut ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(root.0, &registry, NO_INBOUND_SOURCE);

    ctx.send_to(target, &());
    drain_to_members(&registry, "the ctx send");
    assert_eq!(probe.dispatches.get(), 1, "the ctx's own send_to reaches the target");
    assert_eq!(probe.source.get(), Some(root), "and stamps the sending actor as the source");

    ctx.sends().send_to(target, &());
    drain_to_members(&registry, "the view send");
    assert_eq!(probe.dispatches.get(), 2, "the view's send_to reaches the same target");
    assert_eq!(probe.source.get(), Some(root), "and stamps the same source");
}

/// Drain the cluster queue, failing the test if anything reaches the cluster
/// root instead of a member — the shape every leg above expects.
fn drain_to_members(registry: &Registry, leg: &'static str) {
    drain_cluster_queue(registry, move |source| {
        move |_mail| -> u32 { panic!("{leg} unexpectedly reached the cluster root from {source:#x}") }
    });
}

/// `send_to` on the ctx and on its `sends()` view sends through a proven
/// reference — each routes to the reference's id stamped with the sending
/// actor's own id. The legs go through the `Target` impls for `ActorRef<R>`
/// and for a borrow of one, so an impl that forwarded the wrong proof misses
/// the target here, and a routing call that transposed recipient and sender
/// routes to the sender's own id (no dispatch here) and stamps the target as
/// the source; either half fails this test. Synthetic ids keep the fixture off
/// the name fold, so the test needs no suppression.
#[test]
fn send_to_sends_through_a_proven_reference_on_ctx_and_view() {
    use aether_data::MailboxId;

    let registry = Registry::new();
    let root = MailboxId(0x7300);
    registry.set_self_id(root.0);

    let target = MailboxId(0x7301);
    let probe = recording_target();
    registry.insert_child(target, 0, String::from("test.wasm.sends_child"), false, root.0, Vec::new(), probe.actor);

    let mut ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(root.0, &registry, NO_INBOUND_SOURCE);
    let reference = ActorRef::<SendsPeer>::new(target);

    ctx.send_to(reference, &());
    drain_to_members(&registry, "the ctx send_to through a typed reference");
    assert_eq!(probe.dispatches.get(), 1, "the ctx's send_to through the reference reaches its id");
    assert_eq!(probe.source.get(), Some(root), "and stamps the sending actor as the source");

    let borrowed = &reference;
    ctx.sends().send_to(borrowed, &());
    drain_to_members(&registry, "the view send_to through a borrowed reference");
    assert_eq!(probe.dispatches.get(), 2, "the view's send_to through the reference reaches the same id");
    assert_eq!(probe.source.get(), Some(root), "and stamps the same source");
}
