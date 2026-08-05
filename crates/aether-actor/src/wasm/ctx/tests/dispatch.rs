//! What a ctx reads off the dispatch it was built for — the threaded
//! source, the reply correlation, the reply-mode views' layout, the multi
//! class's emit, and the relative verbs' in-place routing.

use super::{NO_INBOUND_SOURCE, Registry, SucceedingChild, WasmCtx, install_inline_child};
use crate::mail::{Mail, PriorState};
use crate::model::ctx::{Emit, Manual, Multi, Single};
use crate::model::{Addressable, CallerScope, CallerScoped, Embedded, HandlesKind, Many, Resolve};
use crate::wasm::inline::{RouteDecision, drain_cluster_queue};
use crate::wasm::{ErasedWasmActor, WasmActorMailbox, WasmDropCtx};
use aether_data::{MailboxId, Source, mailbox_id_from_path};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::mem::{align_of, size_of};

struct EmbeddedPeer;

impl Addressable for EmbeddedPeer {
    const NAMESPACE: &'static str = "test.wasm.embedded_peer";
    type Resolver = Embedded;
}

impl HandlesKind<()> for EmbeddedPeer {}

struct CurrentKeyedPeer;

impl Addressable for CurrentKeyedPeer {
    const NAMESPACE: &'static str = "test.wasm.current_keyed_peer";
    type Resolver = Many;
}

impl HandlesKind<()> for CurrentKeyedPeer {}

struct ParentKeyed;

impl Resolve for ParentKeyed {
    type Args<'a> = &'a str;

    fn resolve(caller_carry: u64, namespace: &str, name: &str) -> MailboxId {
        Many::resolve(caller_carry, namespace, name)
    }
}

impl CallerScoped for ParentKeyed {
    const SCOPE: CallerScope = CallerScope::Parent;
}

struct ParentKeyedPeer;

impl Addressable for ParentKeyedPeer {
    const NAMESPACE: &'static str = "test.wasm.parent_keyed_peer";
    type Resolver = ParentKeyed;
}

impl HandlesKind<()> for ParentKeyedPeer {}

struct RecordingTarget {
    dispatches: Rc<Cell<u32>>,
    source: Rc<Cell<Option<MailboxId>>>,
}

struct RecordingTargetProbe {
    actor: Box<dyn ErasedWasmActor>,
    dispatches: Rc<Cell<u32>>,
    source: Rc<Cell<Option<MailboxId>>>,
}

impl ErasedWasmActor for RecordingTarget {
    fn erased_namespace(&self) -> &'static str {
        "test.wasm.recording_target"
    }

    fn erased_dispatch(&mut self, ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        self.dispatches.set(self.dispatches.get() + 1);
        self.source.set(ctx.source_mailbox());
        0
    }

    fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}

    fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}

    fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}

    fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
}

fn recording_target() -> RecordingTargetProbe {
    let dispatches = Rc::new(Cell::new(0));
    let source = Rc::new(Cell::new(None));
    RecordingTargetProbe {
        actor: Box::new(RecordingTarget { dispatches: Rc::clone(&dispatches), source: Rc::clone(&source) }),
        dispatches,
        source,
    }
}

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

/// Keyed typed construction selects the recipient resolver's declared scope:
/// built-in `Many` folds from the calling actor, while a test-only keyed
/// resolver can fold from its logical parent. Both returned handles retain
/// the ctx's sender and inline registry, proven by local delivery observing
/// the current actor as its source.
#[allow(clippy::disallowed_methods)] // test scaffolding — synthetic lineage IDs exercise scoped routing
#[test]
fn keyed_actor_resolution_selects_scope_and_retains_wasm_context() {
    let registry = Registry::new();
    let parent = mailbox_id_from_path("test.wasm.keyed_parent");
    let current = mailbox_id_from_path("test.wasm.keyed_parent/test.wasm.keyed_caller");
    let current_target = CurrentKeyedPeer::resolve(current.0, "current");
    let parent_target = ParentKeyedPeer::resolve(parent.0, "parent");
    let current_probe = recording_target();
    let parent_probe = recording_target();

    registry.set_self_id(current.0);
    registry.set_parent_id(parent.0);
    registry.insert_child(
        current_target,
        0,
        String::from("current"),
        false,
        current.0,
        Vec::new(),
        current_probe.actor,
    );
    registry.insert_child(parent_target, 0, String::from("parent"), false, parent.0, Vec::new(), parent_probe.actor);

    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(current.0, &registry, NO_INBOUND_SOURCE);
    let current_peer = ctx.resolve_actor::<CurrentKeyedPeer>("current");
    let parent_peer = ctx.resolve_actor::<ParentKeyedPeer>("parent");

    assert_eq!(current_peer.mailbox_id(), current_target, "Many selects the current actor's mailbox");
    assert_eq!(parent_peer.mailbox_id(), parent_target, "the custom keyed resolver selects the logical parent");

    current_peer.send(&());
    parent_peer.send(&());
    drain_cluster_queue(&registry, |source| {
        move |_mail| -> u32 { panic!("keyed target unexpectedly dispatched to cluster root from {source:#x}") }
    });

    assert_eq!(current_probe.dispatches.get(), 1, "the current-scoped handle retains the inline registry");
    assert_eq!(parent_probe.dispatches.get(), 1, "the parent-scoped handle retains the inline registry");
    assert_eq!(current_probe.source.get(), Some(current), "the current-scoped handle retains the ctx sender");
    assert_eq!(parent_probe.source.get(), Some(current), "the parent-scoped handle retains the ctx sender");
}

#[allow(clippy::disallowed_methods)] // test scaffolding — synthetic lineage IDs exercise parent-relative routing
#[test]
fn embedded_actor_resolution_and_delivery_use_entry_and_inline_logical_parents() {
    let registry = Registry::new();
    let entry_parent = mailbox_id_from_path("test.wasm.host");
    let entry = mailbox_id_from_path("test.wasm.host/test.wasm.entry");
    let child = mailbox_id_from_path("test.wasm.host/test.wasm.entry/test.wasm.child");
    let default_entry_peer = Embedded::resolve(entry_parent.0, EmbeddedPeer::NAMESPACE, ());
    let named_entry_peer = Embedded::resolve(entry_parent.0, "named-peer", ());
    let nested_peer = Embedded::resolve(entry.0, EmbeddedPeer::NAMESPACE, ());
    registry.set_self_id(entry.0);
    registry.set_parent_id(entry_parent.0);
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
        named_entry_peer,
        0,
        String::from("named-peer"),
        false,
        entry.0,
        Vec::new(),
        (),
    )
    .expect("install named embedded peer");
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

    let entry_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(entry.0, &registry, NO_INBOUND_SOURCE);
    let child_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(child.0, &registry, NO_INBOUND_SOURCE);

    let default = entry_ctx.actor::<EmbeddedPeer>();
    let named = entry_ctx.__actor_with_namespace::<EmbeddedPeer>("named-peer");
    let nested = child_ctx.actor::<EmbeddedPeer>();
    assert_eq!(default.mailbox_id(), default_entry_peer);
    assert_eq!(named.mailbox_id(), named_entry_peer);
    assert_eq!(nested.mailbox_id(), nested_peer);

    default.send(&());
    named.send(&());
    nested.send(&());
    assert_eq!(registry.queued_len(), 3, "default, named, and nested parent-scoped sends route locally");
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
