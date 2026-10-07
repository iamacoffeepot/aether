//! ADR-0112 / ADR-0134: the mode marker is layout-neutral, and each mode
//! carries exactly the surface its handler class is allowed.

use aether_actor::{Anyone, OutboundReply, Single, Subscriber, Unchecked};
use aether_kinds::Tick;

use crate::actor::native::{Erased, NativeCtx};

/// ADR-0112: the mode marker is layout-neutral — the `Single` and
/// `Unchecked` views have identical size + alignment. This is the
/// invariant the `as_single` pointer reborrow rests on. The sender marker
/// is layout-neutral too, in each mode: the invariant the dispatch arm's
/// `prove_sender` reborrow rests on (ADR-0231 §11).
#[test]
fn native_ctx_layout_identical_across_modes() {
    use std::mem::{align_of, size_of};
    type Proven = Subscriber<Tick>;

    assert_eq!(
        size_of::<NativeCtx<'static, Erased, Anyone, Single>>(),
        size_of::<NativeCtx<'static, Erased, Anyone, Unchecked>>(),
    );
    assert_eq!(
        align_of::<NativeCtx<'static, Erased, Anyone, Single>>(),
        align_of::<NativeCtx<'static, Erased, Anyone, Unchecked>>(),
    );
    assert_eq!(
        size_of::<NativeCtx<'static, Erased, Proven, Single>>(),
        size_of::<NativeCtx<'static, Erased, Anyone, Single>>()
    );
    assert_eq!(
        align_of::<NativeCtx<'static, Erased, Proven, Single>>(),
        align_of::<NativeCtx<'static, Erased, Anyone, Single>>()
    );
    assert_eq!(
        size_of::<NativeCtx<'static, Erased, Proven, Unchecked>>(),
        size_of::<NativeCtx<'static, Erased, Anyone, Unchecked>>()
    );
    assert_eq!(
        align_of::<NativeCtx<'static, Erased, Proven, Unchecked>>(),
        align_of::<NativeCtx<'static, Erased, Anyone, Unchecked>>()
    );
}

/// ADR-0112: `OutboundReply` is reachable from the `Unchecked` ctx
/// only. The single-locked ctx carries no reply surface, so a `-> ()`
/// single handler is provably silent (a stray single-ctx `ctx.reply`
/// is a compile error, not a manifest lie).
#[test]
fn outbound_reply_present_on_unchecked() {
    fn assert_impls<C: OutboundReply>() {}
    assert_impls::<NativeCtx<'static, Erased, Anyone, Unchecked>>();
}
