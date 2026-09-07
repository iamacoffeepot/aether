//! The reply-mode-free send view (`ctx.sends()`): that it addresses and
//! stamps outbound mail exactly as the [`WasmCtx`] it was taken from.

use super::{NO_INBOUND_SOURCE, Registry, WasmCtx, recording_target};
use crate::model::ctx::Manual;
use crate::model::{Addressable, Embedded};
use crate::wasm::inline::drain_cluster_queue;
use aether_data::{MailboxId, mailbox_id_from_path};
use alloc::string::String;
use alloc::vec::Vec;

struct SendsPeer;

impl Addressable for SendsPeer {
    const NAMESPACE: &'static str = "test.wasm.sends_peer";
    type Resolver = Embedded;
}

/// Tripwire: `Sends` carries its own copy of the routing every `WasmCtx` send
/// verb performs, so the two can drift. A view that stamped a different `from`,
/// resolved a different recipient, or bypassed the inline registry would show
/// up here as a missing dispatch or a different observed source.
///
/// The recipient is a cluster member, so the send routes in place and enqueues
/// locally — no host call (the host stub panics on the host build, so reaching
/// the asserts without a panic also proves the local branch). A `()` payload
/// encodes to empty bytes.
#[test]
fn sends_view_routes_and_stamps_like_the_ctx_it_came_from() {
    let registry = Registry::new();
    let root = 0x7300_u64;
    registry.set_self_id(root);

    let target = MailboxId(0x7301);
    let probe = recording_target();
    registry.insert_child(target, 0, String::from("target"), false, root, Vec::new(), probe.actor);

    let mut ctx: WasmCtx<'_, Manual> = WasmCtx::__new(root, &registry, NO_INBOUND_SOURCE);

    ctx.send_to(target, &());
    drain_cluster_queue(&registry, |source| {
        move |_mail| -> u32 { panic!("the ctx send unexpectedly reached the cluster root from {source:#x}") }
    });
    assert_eq!(probe.dispatches.get(), 1, "the ctx's own send_to reaches the target");
    assert_eq!(probe.source.get(), Some(MailboxId(root)), "and stamps the sending actor as the source");

    ctx.sends().send_to(target, &());
    drain_cluster_queue(&registry, |source| {
        move |_mail| -> u32 { panic!("the view send unexpectedly reached the cluster root from {source:#x}") }
    });
    assert_eq!(probe.dispatches.get(), 2, "the view's send_to reaches the same target");
    assert_eq!(probe.source.get(), Some(MailboxId(root)), "and stamps the same source");
}

/// Tripwire: typed resolution through the view walks the same caller scope as
/// the ctx. `Embedded` seeds from the *logical parent*, so a view that seeded
/// from its own mailbox instead — the easy transcription slip — resolves a
/// different id here while the ctx still resolves the right one.
#[allow(clippy::disallowed_methods)] // test scaffolding — synthetic lineage IDs exercise parent-scoped routing
#[test]
fn sends_view_resolves_typed_peers_through_the_same_caller_scope() {
    let registry = Registry::new();
    let parent = mailbox_id_from_path("test.wasm.sends_host");
    let current = mailbox_id_from_path("test.wasm.sends_host/test.wasm.sends_caller");
    registry.set_self_id(current.0);
    registry.set_parent_id(parent.0);

    let mut ctx: WasmCtx<'_, Manual> = WasmCtx::__new(current.0, &registry, NO_INBOUND_SOURCE);

    let through_ctx = ctx.actor::<SendsPeer>().mailbox_id();
    let through_view = ctx.sends().actor::<SendsPeer>().mailbox_id();
    assert_ne!(parent, current, "the fixture's parent and current mailboxes differ, so the scope choice is visible");
    assert_eq!(through_view, through_ctx, "the view resolves the parent-scoped peer the ctx resolves");
}
