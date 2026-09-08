//! The reply-mode-free send view (`ctx.sends()`): that it addresses and
//! stamps outbound mail exactly as the [`WasmCtx`] it was taken from.

use super::{NO_INBOUND_SOURCE, Registry, WasmCtx, recording_target};
use crate::model::ctx::{MailSender, Manual};
use crate::model::{Addressable, Embedded};
use crate::wasm::inline::drain_cluster_queue;
use aether_data::mailbox_id_from_path;
use alloc::string::String;
use alloc::vec::Vec;

struct SendsPeer;

impl Addressable for SendsPeer {
    const NAMESPACE: &'static str = "test.wasm.sends_peer";
    type Resolver = Embedded;
}

/// The child's rendered lineage address — a depth-2 path, so the two folds
/// disagree on it: `mailbox_id_from_path` walks the `/` into two nodes the way
/// the registry does (ADR-0099 §4), while the flat `mailbox_id_from_name`
/// hashes the whole string as one root name and resolves an id nothing
/// registered.
const CHILD_ADDRESS: &str = "test.wasm.sends_host/test.wasm.sends_child";

/// Tripwire: `Sends` carries its own copy of the routing every `WasmCtx` send
/// verb performs, so the two can drift. A view that stamped a different `from`,
/// resolved a different recipient, or bypassed the inline registry would show
/// up here as a missing dispatch or a different observed source.
///
/// The third leg pins the runtime-name fold specifically: `send_to_named` must
/// resolve [`CHILD_ADDRESS`] through the path fold, so the mail reaches the
/// registered child. The flat hasher resolves an unregistered id — the send
/// classifies as cross-cluster and reaches for the host, whose host-build stub
/// panics, and the dispatch count never advances.
///
/// Every recipient here is a cluster member, so each send routes in place and
/// enqueues locally — no host call. A `()` payload encodes to empty bytes.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: fixture registers the child by rendered address
#[test]
fn sends_view_routes_and_stamps_like_the_ctx_it_came_from() {
    let registry = Registry::new();
    let root = mailbox_id_from_path("test.wasm.sends_host");
    registry.set_self_id(root.0);

    let target = mailbox_id_from_path(CHILD_ADDRESS);
    let probe = recording_target();
    registry.insert_child(target, 0, String::from("test.wasm.sends_child"), false, root.0, Vec::new(), probe.actor);

    let mut ctx: WasmCtx<'_, Manual> = WasmCtx::__new(root.0, &registry, NO_INBOUND_SOURCE);

    ctx.send_to(target, &());
    drain_to_members(&registry, "the ctx send");
    assert_eq!(probe.dispatches.get(), 1, "the ctx's own send_to reaches the target");
    assert_eq!(probe.source.get(), Some(root), "and stamps the sending actor as the source");

    ctx.sends().send_to(target, &());
    drain_to_members(&registry, "the view send");
    assert_eq!(probe.dispatches.get(), 2, "the view's send_to reaches the same target");
    assert_eq!(probe.source.get(), Some(root), "and stamps the same source");

    ctx.sends().send_to_named(CHILD_ADDRESS, &());
    drain_to_members(&registry, "the view's by-name send");
    assert_eq!(probe.dispatches.get(), 3, "the view folds a rendered lineage address to the registered child");
    assert_eq!(probe.source.get(), Some(root), "and stamps the same source on the by-name path");
}

/// Drain the cluster queue, failing the test if anything reaches the cluster
/// root instead of a member — the shape every leg above expects.
fn drain_to_members(registry: &Registry, leg: &'static str) {
    drain_cluster_queue(registry, move |source| {
        move |_mail| -> u32 { panic!("{leg} unexpectedly reached the cluster root from {source:#x}") }
    });
}

/// Tripwire: typed resolution through the view walks the same caller scope as
/// the ctx. `Embedded` seeds from the *logical parent*, so a view that seeded
/// from its own mailbox instead — the easy transcription slip — resolves a
/// different id here while the ctx still resolves the right one.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: fixture builds synthetic lineage ids
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
