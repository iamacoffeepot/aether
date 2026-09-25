//! What a ctx reads off the dispatch it was built for — the threaded
//! source, the reply correlation, the reply-mode views' layout, and the
//! relative verbs' in-place routing.

use super::{NO_INBOUND_SOURCE, Registry, SucceedingChild, WasmCtx, install_inline_child};
use crate::mail::Mail;
use crate::model::ctx::{Erased, Manual, Single};
use crate::model::{Addressable, Embedded, HandlesKind, Resolve};
use crate::wasm::inline::RouteDecision;
use crate::wasm::{ActorInitError, WasmInitCtx};
use aether_data::{ActorId, MailboxId, Source};
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{align_of, size_of};

struct EmbeddedPeer;

impl Addressable for EmbeddedPeer {
    const NAMESPACE: &'static str = "test.wasm.embedded_peer";
    type Resolver = Embedded;
}

impl HandlesKind<()> for EmbeddedPeer {}

/// Types the ctx that sends to [`EmbeddedPeer`]: the flat typed verbs exist
/// only on a ctx whose actor declares its recipient.
struct PeerDependent;

#[crate::actor(depends(EmbeddedPeer))]
impl crate::WasmActor for PeerDependent {
    const NAMESPACE: &'static str = "test.wasm.peer_dependent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {
        let _ = self;
    }
}

#[test]
fn local_dispatch_ctx_never_reads_host_reply_correlation() {
    let registry = Registry::new();
    let ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new_local_dispatch(0x10, &registry, NO_INBOUND_SOURCE);
    assert_eq!(ctx.in_reply_to(), None, "cluster-drained dispatches carry no host correlation");
}

/// ADR-0112: the mode marker is layout-neutral — the `Single` and
/// `Manual` views have identical size + alignment. This is the
/// invariant the `as_single` pointer reborrow rests on. The actor marker
/// is layout-neutral too — the invariant the `__for_actor` / `erase`
/// reborrows rest on (issue 6279).
#[test]
fn ffi_ctx_layout_identical_across_modes() {
    assert_eq!(size_of::<WasmCtx<'static, Erased, Single>>(), size_of::<WasmCtx<'static, Erased, Manual>>(),);
    assert_eq!(align_of::<WasmCtx<'static, Erased, Single>>(), align_of::<WasmCtx<'static, Erased, Manual>>(),);
    assert_eq!(size_of::<WasmCtx<'static, EmbeddedPeer, Manual>>(), size_of::<WasmCtx<'static, Erased, Manual>>(),);
    assert_eq!(align_of::<WasmCtx<'static, EmbeddedPeer, Manual>>(), align_of::<WasmCtx<'static, Erased, Manual>>(),);
}

#[test]
fn embedded_actor_resolution_and_delivery_use_entry_and_inline_logical_parents() {
    let registry = Registry::new();
    let entry_parent = ActorId::singleton("test.wasm.host").0;
    let entry = Embedded::resolve(entry_parent, "test.wasm.entry", ());
    let child = Embedded::resolve(entry.0, "test.wasm.child", ());
    let default_entry_peer = Embedded::resolve(entry_parent, EmbeddedPeer::NAMESPACE, ());
    let nested_peer = Embedded::resolve(entry.0, EmbeddedPeer::NAMESPACE, ());
    registry.set_self_id(entry.0);
    registry.set_parent_id(entry_parent);
    install_inline_child::<SucceedingChild>(&registry, child, 0, String::from("child"), false, entry.0, Vec::new(), ())
        .expect("install inline child");
    install_inline_child::<SucceedingChild>(
        &registry,
        default_entry_peer,
        0,
        String::from("default-peer"),
        false,
        entry.0,
        Vec::new(),
        (),
    )
    .expect("install default embedded peer");
    install_inline_child::<SucceedingChild>(
        &registry,
        nested_peer,
        0,
        String::from("nested-peer"),
        false,
        entry.0,
        Vec::new(),
        (),
    )
    .expect("install nested embedded peer");

    let mut entry_ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(entry.0, &registry, NO_INBOUND_SOURCE);
    let mut child_ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(child.0, &registry, NO_INBOUND_SOURCE);
    let entry_ctx = entry_ctx.__for_actor::<PeerDependent>();
    let child_ctx = child_ctx.__for_actor::<PeerDependent>();

    assert_eq!(entry_ctx.actor_ref::<EmbeddedPeer>().id(), default_entry_peer);
    assert_eq!(child_ctx.actor_ref::<EmbeddedPeer>().id(), nested_peer);

    entry_ctx.send::<EmbeddedPeer>(&());
    child_ctx.send::<EmbeddedPeer>(&());
    assert_eq!(registry.queued_len(), 2, "default and nested parent-scoped sends route locally");
}

/// ADR-0114 addressing amendment: a ctx self-identified as the cluster
/// root resolves `child(name)` to the resident inline child, returns
/// `None` for a missing name, and a send through the resolved relative
/// routes in place (enqueues locally — no host call, which would panic
/// on the host build). `parent()` of the root is `None` (cross-cluster).
#[test]
fn ctx_relative_verbs_resolve_and_route_in_place() {
    let registry = Registry::new();
    let root = 0x7100_u64;
    registry.set_self_id(root);
    // Install a child of the root keyed by a synthetic alias, then a
    // grandchild under it. Record each parent the way `spawn_inline_child`
    // would.
    let widget = MailboxId(0x7101);
    let label = MailboxId(0x7102);
    install_inline_child::<SucceedingChild>(&registry, widget, 0, String::from("widget"), false, root, Vec::new(), ())
        .expect("a succeeding init installs the inline child");
    install_inline_child::<SucceedingChild>(
        &registry,
        label,
        0,
        String::from("label"),
        false,
        widget.0,
        Vec::new(),
        (),
    )
    .expect("a succeeding init installs the inline grandchild");

    let ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(root, &registry, NO_INBOUND_SOURCE);

    // The root has no registry parent entry — its parent is cross-cluster.
    assert!(ctx.parent().is_none(), "the cluster root resolves no in-cluster parent");

    // child(name) resolves the resident widget; a missing name is None.
    let child = ctx.child("widget").expect("the widget resolves by subname");
    assert_eq!(child.id, widget, "child resolves to the alias id");
    assert!(ctx.child("missing").is_none(), "a missing subname resolves to None");
    let grandchild = child.child("label").expect("the grandchild resolves relative to the child handle");
    assert_eq!(grandchild.id, label, "handle-relative child walk reaches the grandchild");
    assert!(child.child("missing").is_none(), "a missing grandchild segment resolves to None");

    // The resolved relative is a cluster member, so a send routes in
    // place; the local path enqueues and makes no host call (the host
    // stub panics on the host build, so reaching this line without a
    // panic proves the send took the local branch). A `()` payload
    // encodes to empty bytes.
    assert_eq!(
        registry.route_decision(child.id.0),
        RouteDecision::Local,
        "the resolved relative is classified as an in-cluster recipient",
    );
    child.send(&());
    assert_eq!(registry.queued_len(), 1, "a send to a resolved relative enqueues locally — no scheduler hop");
}

/// A tracked send to a resident cluster member never leaves the guest, so it
/// enqueues in place and returns the no-correlation sentinel instead of a
/// stale host correlation. Owned logic: the local branch of the tracked-send
/// routing.
#[test]
fn send_tracked_local_route_enqueues_and_returns_no_correlation() {
    let registry = Registry::new();
    let parent = 0x7000_u64;
    let root = 0x7100_u64;
    registry.set_self_id(root);
    registry.set_parent_id(parent);
    let peer = Embedded::resolve(parent, EmbeddedPeer::NAMESPACE, ());
    install_inline_child::<SucceedingChild>(&registry, peer, 0, String::from("peer"), false, root, Vec::new(), ())
        .expect("install inline child");

    let mut ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(root, &registry, NO_INBOUND_SOURCE);
    let request = ctx.__for_actor::<PeerDependent>().send_tracked::<EmbeddedPeer>(&());
    assert_eq!(request.0, Source::NO_CORRELATION, "local inline sends have no host-minted request id");
    assert_eq!(registry.queued_len(), 1, "local tracked sends enqueue their payload before returning the sentinel");
}
