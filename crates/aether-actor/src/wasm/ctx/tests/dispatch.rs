//! What a ctx reads off the dispatch it was built for — the threaded
//! source, the reply correlation, the reply-mode views' layout, and the
//! relative verbs' in-place routing.

use super::{NO_INBOUND_SOURCE, Registry, SucceedingChild, WasmCtx, install_inline_child};
use crate::mail::{Mail, NO_REPLY_HANDLE};
use crate::model::ctx::{Erased, Manual, Single};
use crate::model::{Addressable, HandlesKind, One, Resolve};
use crate::reference::ErasedActorRef;
use crate::wasm::inline::{ChildRecord, RouteDecision};
use crate::wasm::{ActorInitError, WasmInitCtx};
use aether_data::{Kind, MailboxId, Source};
use alloc::string::String;
use core::mem::{self, align_of, size_of};

struct RootPeer;

impl Addressable for RootPeer {
    const NAMESPACE: &'static str = "test.wasm.root_peer";
    type Resolver = One;
}

impl HandlesKind<()> for RootPeer {}

/// Types the ctx that sends to [`RootPeer`]: the flat typed verbs exist
/// only on a ctx whose actor declares its recipient.
struct PeerDependent;

#[crate::actor(depends(RootPeer))]
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

#[aether_data::kind(name = "test.wasm.deferred_ask")]
struct DeferredAsk {
    value: u32,
}

#[aether_data::kind(name = "test.wasm.deferred_answer")]
struct DeferredAnswer {
    value: u32,
}

impl crate::HeldReply for DeferredAnswer {
    fn unanswered() -> Self {
        Self { value: 0 }
    }
}

/// Answers [`DeferredAsk`] later: its handler holds the reply, parks the
/// ticket in its state and returns the receipt.
struct Deferrer {
    parked: Option<crate::Held<DeferredAnswer>>,
}

#[crate::actor]
impl crate::WasmActor for Deferrer {
    const NAMESPACE: &'static str = "test.wasm.deferrer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { parked: None })
    }

    #[handler::single]
    fn on_ask(&mut self, ctx: &mut WasmCtx<'_>, _ask: DeferredAsk) -> crate::Pending<DeferredAnswer> {
        let (pending, held) = ctx.hold::<DeferredAnswer>();
        self.parked = Some(held);
        pending
    }
}

/// ADR-0243 §6: a single `-> Pending<R>` arm reports `DISPATCH_HANDLED_HOLD`,
/// so the host keeps the reply handle its `Held` answers through. An arm
/// that reported `DISPATCH_HANDLED_RELEASE` would free the handle under the
/// debt, and one that failed to accept the receipt would trap.
#[test]
fn deferred_arm_returns_hold() {
    let registry = Registry::new();
    let mut deferrer = Deferrer { parked: None };
    let payload = DeferredAsk { value: 1 }.encode_into_bytes();
    let handle = 9;
    // SAFETY: `payload` outlives the `Mail` built over it.
    let mail =
        unsafe { Mail::__from_ptr(DeferredAsk::ID.0, payload.as_ptr().addr(), payload.len() as u32, 1, handle, 0x10) };

    let mut ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let rc = <Deferrer as crate::WasmDispatch<Deferrer>>::dispatch(&mut deferrer, &mut ctx, mail);
    assert_eq!(rc, crate::DISPATCH_HANDLED_HOLD);

    let held = deferrer.parked.take().expect("the handler parked its ticket");
    registry.release_held(handle);
    assert_ne!(handle, NO_REPLY_HANDLE);
    mem::forget(held);
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
    assert_eq!(size_of::<WasmCtx<'static, RootPeer, Manual>>(), size_of::<WasmCtx<'static, Erased, Manual>>(),);
    assert_eq!(align_of::<WasmCtx<'static, RootPeer, Manual>>(), align_of::<WasmCtx<'static, Erased, Manual>>(),);
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
    install_inline_child::<SucceedingChild>(
        &registry,
        widget,
        ChildRecord { full_subname: String::from("widget"), parent: root, ..ChildRecord::default() },
        (),
    )
    .expect("a succeeding init installs the inline child");
    install_inline_child::<SucceedingChild>(
        &registry,
        label,
        ChildRecord { full_subname: String::from("label"), parent: widget.0, ..ChildRecord::default() },
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
    assert_eq!(child.reference(), ErasedActorRef::new(widget), "the relative's proof names the resolved child");
    assert_ne!(
        child.reference(),
        ErasedActorRef::new(MailboxId(root)),
        "the relative's proof is not the addresser's own",
    );
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
    let root = 0x7100_u64;
    registry.set_self_id(root);
    let peer = One::resolve(0, RootPeer::NAMESPACE, ());
    install_inline_child::<SucceedingChild>(
        &registry,
        peer,
        ChildRecord { full_subname: String::from("peer"), parent: root, ..ChildRecord::default() },
        (),
    )
    .expect("install inline child");

    let mut ctx: WasmCtx<'_, Erased, Manual> = WasmCtx::__new(root, &registry, NO_INBOUND_SOURCE);
    let request = ctx.__for_actor::<PeerDependent>().send_tracked::<RootPeer>(&());
    assert_eq!(request.0, Source::NO_CORRELATION, "local inline sends have no host-minted request id");
    assert_eq!(registry.queued_len(), 1, "local tracked sends enqueue their payload before returning the sentinel");
}
