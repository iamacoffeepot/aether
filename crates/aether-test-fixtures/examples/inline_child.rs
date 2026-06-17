//! ADR-0114 inline-child fixture. A single-actor module whose entry
//! `InlineParent` spawns a co-located `InlineChild` in `wire` via
//! `ctx.spawn_inline_child::<InlineChild>` (ADR-0114). The child gets a
//! first-class lineage address (`{parent}/aether.embedded:widget`) routed
//! to the parent's one slot; the `export!` membrane demuxes mail
//! addressed to that alias to the child.
//!
//! Both actors answer the same `InlineProbe` query with an `InlineEcho`
//! tagged by `who` handled it, so a `FleetBench` scenario can send to the
//! child's address over the real wire and assert the *child* (not the
//! parent) replied — and that a control send to the parent's own address
//! is unaffected (the membrane no-ops to the parent). `InlineChild` is
//! `Instanced` (the `spawn_inline_child` bound); it is not in the
//! `export!` list because an inline child is constructed in-process by
//! the parent, not instantiated by the host.

// `#[handler::manual]` methods take `&mut self` to match the dispatch ABI
// even though these stateless replies never read it.
#![allow(clippy::unused_self)]

use aether_actor::{
    BootError, FfiActor, FfiCtx, FfiInitCtx, Instanced, Manual, OutboundReply, Subname, actor,
};
use aether_test_fixtures::{INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho, InlineProbe};

/// Reply to an `InlineProbe` with the `who` marker of whichever actor
/// handled it — shared by the parent's own-id (control) path and the
/// child's demux path — when the probe carries a reply target.
fn reply_who(ctx: &mut FfiCtx<'_, Manual>, who: u32) {
    if ctx.reply_target().is_some() {
        ctx.reply(&InlineEcho { who });
    }
}

/// Entry export — the loaded component. Spawns its inline child in `wire`.
pub struct InlineParent;

#[actor]
impl FfiActor for InlineParent {
    const NAMESPACE: &'static str = "test.inline.parent";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineParent)
    }

    /// ADR-0114: co-locate an `InlineChild` under the `Named` subname
    /// `widget`. The returned alias `MailboxId` is fire-and-forget here —
    /// the `FleetBench` addresses the child by its rendered lineage name.
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        let _ = ctx.spawn_inline_child::<InlineChild>(Subname::Named("widget"), &());
    }

    /// Answer an `InlineProbe` addressed to the parent's own mailbox with
    /// the parent marker — the membrane's own-id (control) path.
    #[handler::manual]
    fn on_probe(&mut self, ctx: &mut FfiCtx<'_, Manual>, _probe: InlineProbe) {
        reply_who(ctx, INLINE_WHO_PARENT);
    }
}

/// Inline child — co-located in the parent's wasm instance. `Instanced`
/// so it satisfies the `spawn_inline_child` bound; not exported (the
/// parent constructs it in-process).
pub struct InlineChild;

impl Instanced for InlineChild {}

#[actor]
impl FfiActor for InlineChild {
    const NAMESPACE: &'static str = "test.inline.child";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineChild)
    }

    /// Answer an `InlineProbe` addressed to the child's alias with the
    /// child marker — the membrane's child-demux path.
    #[handler::manual]
    fn on_probe(&mut self, ctx: &mut FfiCtx<'_, Manual>, _probe: InlineProbe) {
        reply_who(ctx, INLINE_WHO_CHILD);
    }
}

aether_actor::export!(InlineParent);
