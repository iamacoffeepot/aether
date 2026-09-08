//! ADR-0112 / ADR-0134: the mode marker is layout-neutral, and each mode
//! carries exactly the surface its handler class is allowed.

use aether_actor::{Emit, Manual, Multi, OutboundReply, Single};

use crate::actor::native::NativeCtx;

use super::support::CastOnly;

/// ADR-0112: the mode marker is layout-neutral — the `Single` and
/// `Manual` views have identical size + alignment. This is the
/// invariant the `as_single` pointer reborrow rests on.
#[test]
fn native_ctx_layout_identical_across_modes() {
    use std::mem::{align_of, size_of};
    assert_eq!(size_of::<NativeCtx<'static, Single>>(), size_of::<NativeCtx<'static, Manual>>(),);
    assert_eq!(align_of::<NativeCtx<'static, Single>>(), align_of::<NativeCtx<'static, Manual>>(),);
}

/// ADR-0112: `OutboundReply` is reachable from the `Manual` ctx
/// only. The single-locked ctx carries no reply surface, so a `-> ()`
/// single handler is provably silent (a stray single-ctx `ctx.reply`
/// is a compile error, not a manifest lie).
#[test]
fn outbound_reply_present_on_manual() {
    fn assert_impls<C: OutboundReply>() {}
    assert_impls::<NativeCtx<'static, Manual>>();
}

/// ADR-0134: the multi mode marker is layout-neutral — a `Multi<K>`
/// view has the same size + alignment as the `Single` view. This is the
/// invariant the `as_multi` pointer reborrow rests on.
#[test]
fn native_ctx_layout_identical_for_multi_mode() {
    use std::mem::{align_of, size_of};
    assert_eq!(size_of::<NativeCtx<'static, Single>>(), size_of::<NativeCtx<'static, Multi<u32>>>(),);
    assert_eq!(align_of::<NativeCtx<'static, Single>>(), align_of::<NativeCtx<'static, Multi<u32>>>(),);
}

/// ADR-0134: `Emit` is reachable from the `Multi<K>` ctx only. The
/// single- / manual-locked ctxs carry no emit surface, so a stray
/// `ctx.emit` outside a multi handler is a compile error, not a
/// manifest lie.
#[test]
fn emit_present_on_multi() {
    fn assert_impls<C: Emit<CastOnly>>() {}
    assert_impls::<NativeCtx<'static, Multi<CastOnly>>>();
}
