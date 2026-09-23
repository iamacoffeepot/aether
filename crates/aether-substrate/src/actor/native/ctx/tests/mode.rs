//! ADR-0112 / ADR-0134: the mode marker is layout-neutral, and each mode
//! carries exactly the surface its handler class is allowed.

use aether_actor::{Manual, OutboundReply, Single};

use crate::actor::native::{Erased, NativeCtx};

/// ADR-0112: the mode marker is layout-neutral — the `Single` and
/// `Manual` views have identical size + alignment. This is the
/// invariant the `as_single` pointer reborrow rests on.
#[test]
fn native_ctx_layout_identical_across_modes() {
    use std::mem::{align_of, size_of};
    assert_eq!(size_of::<NativeCtx<'static, Erased, Single>>(), size_of::<NativeCtx<'static, Erased, Manual>>(),);
    assert_eq!(align_of::<NativeCtx<'static, Erased, Single>>(), align_of::<NativeCtx<'static, Erased, Manual>>(),);
}

/// ADR-0112: `OutboundReply` is reachable from the `Manual` ctx
/// only. The single-locked ctx carries no reply surface, so a `-> ()`
/// single handler is provably silent (a stray single-ctx `ctx.reply`
/// is a compile error, not a manifest lie).
#[test]
fn outbound_reply_present_on_manual() {
    fn assert_impls<C: OutboundReply>() {}
    assert_impls::<NativeCtx<'static, Erased, Manual>>();
}
