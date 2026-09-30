//! What a ctx reads off the dispatch it was built for — the threaded
//! source, the reply correlation, the reply-mode views' layout, and the
//! typed parent door's in-place routing.

use super::{ActorTypeTag, NO_INBOUND_SOURCE, Registry, SucceedingChild, WasmCtx, install_inline_child};
use crate::mail::{Mail, NO_REPLY_HANDLE};
use crate::model::ctx::{Erased, Single, Unchecked};
use crate::model::{Addressable, One};
use crate::reference::ErasedActorRef;
use crate::wasm::inline::{ChildRecord, RouteDecision};
use crate::wasm::{ActorInitError, WasmInitCtx};
use aether_data::{Kind, MailboxId};
use alloc::string::String;
use core::mem::{self, align_of, size_of};

struct RootPeer;

impl Addressable for RootPeer {
    const NAMESPACE: &'static str = "test.wasm.root_peer";
    type Resolver = One;
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

    #[handler::request]
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

    let mut ctx: WasmCtx<'_, Erased, Unchecked> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let rc = <Deferrer as crate::WasmDispatch<Deferrer>>::dispatch(&mut deferrer, &mut ctx, mail);
    assert_eq!(rc, crate::DISPATCH_HANDLED_HOLD);

    let held = deferrer.parked.take().expect("the handler parked its ticket");
    registry.release_held(handle);
    assert_ne!(handle, NO_REPLY_HANDLE);
    mem::forget(held);
}

#[repr(C)]
#[aether_data::kind(name = "test.wasm.strict_probe.poke", pod)]
struct Poke {
    seq: u32,
}

/// A strict receiver (no `#[fallback]`) probing whether its one handler ran.
struct StrictProbe {
    ran: bool,
}

#[crate::actor]
impl crate::WasmActor for StrictProbe {
    const NAMESPACE: &'static str = "test.wasm.strict_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { ran: false })
    }

    #[handler::tell]
    fn on_poke(&mut self, _ctx: &mut WasmCtx<'_>, _poke: Poke) {
        self.ran = true;
    }
}

/// iamacoffeepot/aether#2455 regression: the macro-generated dispatch arm
/// must decode-check before reporting handled, so a recognized kind id with
/// an undecodable payload falls through to the strict tail
/// (`DISPATCH_UNKNOWN_KIND`) rather than the handler's `DISPATCH_HANDLED`.
/// Pre-fix the arm returned `DISPATCH_HANDLED` unconditionally once the kind
/// id matched, so a corrupt/truncated payload for a known kind reported
/// success while the handler never ran and no reply was emitted — diverging
/// from the native arm (which routes the same case to the fallback /
/// unknown-kind path) and hanging a request-shaped caller to its settlement
/// timeout with no diagnostic.
///
/// Drives the same generated dispatch table `receive_p32` calls
/// (`WasmDispatch::dispatch`), the way `deferred_arm_returns_hold` does, over
/// a strict `Poke` receiver: `Poke` is a `#[repr(C)]` 4-byte cast-shape kind
/// (`seq: u32`), and a 2-byte payload fails the cast decoder's
/// `len() == size_of` check, so the matched arm's `decode_kind::<Poke>()` is
/// `None`.
#[test]
fn undecodable_payload_for_a_known_kind_falls_to_the_strict_tail() {
    let registry = Registry::new();
    let mut probe = StrictProbe { ran: false };
    let payload = [0u8, 0u8];
    // SAFETY: `payload` outlives the `Mail` built over it.
    let mail = unsafe {
        Mail::__from_ptr(Poke::ID.0, payload.as_ptr().addr(), payload.len() as u32, 1, NO_REPLY_HANDLE, 0x10)
    };

    let mut ctx: WasmCtx<'_, Erased, Unchecked> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let rc = <StrictProbe as crate::WasmDispatch<StrictProbe>>::dispatch(&mut probe, &mut ctx, mail);

    // Tripwire: pre-fix the arm returned `DISPATCH_HANDLED` (0) once the kind
    // id matched, regardless of decode outcome; post-fix the failed decode
    // falls through to the strict tail → `DISPATCH_UNKNOWN_KIND` (1).
    assert_eq!(
        rc,
        crate::DISPATCH_UNKNOWN_KIND,
        "a recognized kind with an undecodable payload must fall through to the tail \
         (DISPATCH_UNKNOWN_KIND), not report DISPATCH_HANDLED",
    );
    assert_ne!(rc, crate::DISPATCH_HANDLED);
    assert!(!probe.ran, "the handler must not run over an undecodable payload");
}

#[test]
fn local_dispatch_ctx_never_reads_host_reply_correlation() {
    let registry = Registry::new();
    let ctx: WasmCtx<'_, Erased, Unchecked> = WasmCtx::__new_local_dispatch(0x10, &registry, NO_INBOUND_SOURCE);
    assert_eq!(ctx.in_reply_to(), None, "cluster-drained dispatches carry no host correlation");
}

/// ADR-0112: the mode marker is layout-neutral — the `Single` and
/// `Unchecked` views have identical size + alignment. This is the
/// invariant the `as_single` pointer reborrow rests on. The actor marker
/// is layout-neutral too — the invariant the `__for_actor` / `erase`
/// reborrows rest on (issue 6279).
#[test]
fn ffi_ctx_layout_identical_across_modes() {
    assert_eq!(size_of::<WasmCtx<'static, Erased, Single>>(), size_of::<WasmCtx<'static, Erased, Unchecked>>(),);
    assert_eq!(align_of::<WasmCtx<'static, Erased, Single>>(), align_of::<WasmCtx<'static, Erased, Unchecked>>(),);
    assert_eq!(size_of::<WasmCtx<'static, RootPeer, Unchecked>>(), size_of::<WasmCtx<'static, Erased, Unchecked>>(),);
    assert_eq!(align_of::<WasmCtx<'static, RootPeer, Unchecked>>(), align_of::<WasmCtx<'static, Erased, Unchecked>>(),);
}

/// ADR-0114 addressing amendment: a ctx typed by a child-only actor resolves
/// `parent()` to the parent the registry recorded at install, and a send
/// through it routes in place (enqueues locally — no host call, which would
/// panic on the host build). The root reaches the child through the
/// tag-checked `child_as`.
#[test]
fn ctx_parent_resolves_and_routes_in_place() {
    let registry = Registry::new();
    let root = 0x7100_u64;
    registry.set_self_id(root);
    // Install a child of the root keyed by a synthetic alias, recording its
    // parent the way `spawn_inline_child` would.
    let widget = MailboxId(0x7101);
    install_inline_child::<SucceedingChild>(
        &registry,
        widget,
        ChildRecord {
            type_tag: ActorTypeTag::of::<SucceedingChild>().0,
            full_subname: String::from("widget"),
            parent: root,
            ..ChildRecord::default()
        },
        (),
    )
    .expect("a succeeding init installs the inline child");

    let root_ctx: WasmCtx<'_, Erased, Unchecked> = WasmCtx::__new(root, &registry, NO_INBOUND_SOURCE);
    let child = root_ctx.child_as::<SucceedingChild>("widget").expect("the widget resolves by subname and tag");
    assert_eq!(child.id(), widget, "child_as resolves to the alias id");

    let mut child_ctx: WasmCtx<'_, Erased, Unchecked> = WasmCtx::__new(widget.0, &registry, NO_INBOUND_SOURCE);
    let parent = child_ctx.__for_actor::<SucceedingChild>().parent();
    assert_eq!(parent.reference(), ErasedActorRef::new(MailboxId(root)), "the parent resolves to the recorded parent");

    // The recorded parent is the cluster root, so a send routes in place;
    // the local path enqueues and makes no host call (the host stub panics
    // on the host build, so reaching this line without a panic proves the
    // send took the local branch). A `()` payload encodes to empty bytes.
    assert_eq!(
        registry.route_decision(root),
        RouteDecision::Local,
        "the resolved parent is classified as an in-cluster recipient",
    );
    parent.send(&());
    assert_eq!(registry.queued_len(), 1, "a send to the parent enqueues locally — no scheduler hop");
}
