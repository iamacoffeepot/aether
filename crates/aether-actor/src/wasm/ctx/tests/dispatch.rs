//! What a ctx reads off the dispatch it was built for — the threaded
//! source, the reply correlation, the reply-mode views' layout, the multi
//! class's emit, and the relative verbs' in-place routing.

use super::{NO_INBOUND_SOURCE, Registry, SucceedingChild, WasmCtx, install_inline_child};
use crate::model::ctx::{Emit, Manual, Multi, Single};
use crate::wasm::WasmActorMailbox;
use crate::wasm::inline::RouteDecision;
use aether_data::{MailboxId, Source};
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{align_of, size_of};

/// Issue 2001: `source_mailbox()` is a single read of the ctx's
/// `source` field on the top-level path — the host threads the resolved
/// inbound source over the `receive_p32` ABI and the `export!` membrane
/// hands it to `__new` (the same field the in-place drain threads). A
/// non-`NONE` source yields `Some(id)`; `NONE` (the no-peer-origin
/// sentinel) yields `None`. No host round-trip is involved.
#[test]
fn source_mailbox_reads_the_threaded_source_field() {
    let registry = Registry::new();

    let source = MailboxId(0x9999_0000_1234_5678);
    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, source.0);
    assert_eq!(ctx.source_mailbox(), Some(source), "a non-NONE threaded source must surface verbatim");

    let none_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    assert_eq!(none_ctx.source_mailbox(), None, "MailboxId::NONE means no peer-component origin");
}

#[test]
fn local_dispatch_ctx_never_reads_host_reply_correlation() {
    let registry = Registry::new();
    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new_local_dispatch(0x10, &registry, NO_INBOUND_SOURCE);
    assert_eq!(ctx.in_reply_to(), None, "cluster-drained dispatches carry no host correlation");
}

/// ADR-0134: `emit` on a `Multi<K>` ctx routes a detached mail at the
/// threaded dispatch source, and a sourceless dispatch drops the
/// emission. The source is set to a cluster member (the self id) so the
/// detached route resolves in place and enqueues locally — no host call
/// (the host stub panics on the host build, so reaching the assert
/// without a panic proves the local branch). A `()` payload encodes to
/// empty bytes.
#[test]
fn emit_routes_at_the_threaded_source_and_drops_when_sourceless() {
    let registry = Registry::new();
    let source = 0x7200_u64;
    registry.set_self_id(source);

    // A dispatch whose source is a cluster member: emit routes a
    // detached mail there and enqueues locally.
    let mut ctx: WasmCtx<'_, Manual> = WasmCtx::__new(source, &registry, source);
    Emit::<()>::emit(ctx.as_multi::<()>(), &());
    assert_eq!(registry.queued_len(), 1, "emit routes a detached mail at the threaded source");

    // A sourceless dispatch (NONE) has no routable target — the emit
    // drops rather than enqueuing.
    let mut none_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(source, &registry, NO_INBOUND_SOURCE);
    Emit::<()>::emit(none_ctx.as_multi::<()>(), &());
    assert_eq!(registry.queued_len(), 1, "a sourceless emit drops — no additional mail enqueued");
}

/// ADR-0134: the multi mode marker is layout-neutral — a `Multi<K>`
/// view has the same size + alignment as the `Single` / `Manual` views.
/// This is the invariant the `as_multi` pointer reborrow rests on.
#[test]
fn ffi_ctx_layout_identical_for_multi_mode() {
    assert_eq!(size_of::<WasmCtx<'static, Single>>(), size_of::<WasmCtx<'static, Multi<u32>>>(),);
    assert_eq!(align_of::<WasmCtx<'static, Single>>(), align_of::<WasmCtx<'static, Multi<u32>>>(),);
}

/// ADR-0112: the mode marker is layout-neutral — the `Single` and
/// `Manual` views have identical size + alignment. This is the
/// invariant the `as_single` pointer reborrow rests on.
#[test]
fn ffi_ctx_layout_identical_across_modes() {
    assert_eq!(size_of::<WasmCtx<'static, Single>>(), size_of::<WasmCtx<'static, Manual>>(),);
    assert_eq!(align_of::<WasmCtx<'static, Single>>(), align_of::<WasmCtx<'static, Manual>>(),);
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

    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(root, &registry, NO_INBOUND_SOURCE);

    // The root has no registry parent entry — its parent is cross-cluster.
    assert!(ctx.parent().is_none(), "the cluster root resolves no in-cluster parent");

    // child(name) resolves the resident widget; a missing name is None.
    let child = ctx.child("widget").expect("the widget resolves by subname");
    assert_eq!(child.mailbox_id(), widget, "child resolves to the alias id");
    assert!(ctx.child("missing").is_none(), "a missing subname resolves to None");
    let grandchild = child.child("label").expect("the grandchild resolves relative to the child handle");
    assert_eq!(grandchild.mailbox_id(), label, "handle-relative child walk reaches the grandchild");
    assert!(child.child("missing").is_none(), "a missing grandchild segment resolves to None");

    // The resolved relative is a cluster member, so a send routes in
    // place; the local path enqueues and makes no host call (the host
    // stub panics on the host build, so reaching this line without a
    // panic proves the send took the local branch). A `()` payload
    // encodes to empty bytes.
    assert_eq!(
        registry.route_decision(child.mailbox_id().0),
        RouteDecision::Local,
        "the resolved relative is classified as an in-cluster recipient",
    );
    child.send(&());
    assert_eq!(registry.queued_len(), 1, "a send to a resolved relative enqueues locally — no scheduler hop");
}

#[test]
fn send_tracked_local_route_enqueues_and_returns_no_correlation() {
    let registry = Registry::new();
    let root = 0x7100_u64;
    registry.set_self_id(root);
    let child = MailboxId(0x7101);
    install_inline_child::<SucceedingChild>(&registry, child, 0, String::from("widget"), false, root, Vec::new(), ())
        .expect("install inline child");

    let mailbox = WasmActorMailbox::<SucceedingChild>::__new(child.0, root, &registry);
    let request = mailbox.send_tracked(&());
    assert_eq!(request.0, Source::NO_CORRELATION, "local inline sends have no host-minted request id");
    assert_eq!(registry.queued_len(), 1, "local tracked sends enqueue their payload before returning the sentinel");
}
