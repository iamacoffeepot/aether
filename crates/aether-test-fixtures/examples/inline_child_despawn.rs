//! ADR-0114 inline-child teardown fixture (#1939). A single-actor module
//! whose entry `InlineDespawnParent` spawns a co-located `InlineDespawnChild`
//! in `wire` via `ctx.spawn_inline_child` and **stores the returned alias**,
//! so a `DespawnChild` trigger to the parent tears the child down via
//! `ctx.despawn_inline_child(self.child)`.
//!
//! Both actors answer the shared `InlineProbe` with a `who`-tagged
//! `InlineEcho`, so a `TestBench` scenario can probe the child's alias (the
//! child answers + settles), tear the child down, then probe the **same**
//! alias again and assert the *parent* answered (the membrane fell through
//! to the parent's dispatch tail) and the chain still settled — the
//! post-teardown-settlement guarantee. The substrate alias route is kept on
//! teardown, so the orphaned mail settles through the parent rather than
//! being short-circuit-dropped.
//!
//! `InlineDespawnChild` also handles `DespawnChild` by despawning *itself*
//! mid-dispatch (the reentrant self-despawn path), addressed by the child's
//! own alias (`ctx.mailbox_id()`). It is `Instanced` (the
//! `spawn_inline_child` bound) and not in the `export!` list — an inline
//! child is constructed in-process by the parent, not instantiated by the
//! host. A sibling of `inline_child.rs` so the #1916 `FleetBench` fixture
//! stays pristine.

// `#[handler::manual]` methods take `&mut self` to match the dispatch ABI
// even though these stateless replies never read it.
#![allow(clippy::unused_self)]

use aether_actor::{
    BootError, FfiActor, FfiCtx, FfiInitCtx, Instanced, Manual, OutboundReply, Subname, actor,
};
use aether_data::MailboxId;
use aether_test_fixtures::{
    DespawnChild, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho, InlineProbe,
};

/// Reply to an `InlineProbe` with the `who` marker of whichever actor
/// handled it — shared by the parent's own-id (control / post-teardown
/// fallthrough) path and the child's demux path — when the probe carries a
/// reply target.
fn reply_who(ctx: &mut FfiCtx<'_, Manual>, who: u32) {
    if ctx.reply_target().is_some() {
        ctx.reply(&InlineEcho { who });
    }
}

/// Entry export — the loaded component. Spawns its inline child in `wire`,
/// stores the alias, and tears the child down on a `DespawnChild` trigger.
pub struct InlineDespawnParent {
    /// The spawned child's alias `MailboxId` (set in `wire`), the handle the
    /// `DespawnChild` handler tears down. `None` until `wire` runs.
    child: Option<MailboxId>,
}

#[actor]
impl FfiActor for InlineDespawnParent {
    const NAMESPACE: &'static str = "test.inline.despawn_parent";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineDespawnParent { child: None })
    }

    /// ADR-0114: co-locate an `InlineDespawnChild` under the `Named` subname
    /// `widget` and store the returned alias so the `DespawnChild` handler
    /// can tear it down. The child is addressed by its rendered lineage name
    /// (`{parent}/aether.embedded:widget`).
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        if let Ok(alias) =
            ctx.spawn_inline_child::<InlineDespawnChild>(Subname::Named("widget"), &())
        {
            self.child = Some(alias);
        }
    }

    /// Tear down the stored inline child (ADR-0114 teardown). The substrate
    /// alias route is kept, so a later probe to the now-dead alias settles
    /// back through this parent's dispatch tail rather than leaking.
    #[handler::manual]
    fn on_despawn(&mut self, ctx: &mut FfiCtx<'_, Manual>, _trigger: DespawnChild) {
        if let Some(child) = self.child {
            let _ = ctx.despawn_inline_child(child);
        }
    }

    /// Answer an `InlineProbe` addressed to the parent's own mailbox with
    /// the parent marker — the membrane's own-id (control) path, and the
    /// post-teardown fallthrough target for a probe to the dead child alias.
    #[handler::manual]
    fn on_probe(&mut self, ctx: &mut FfiCtx<'_, Manual>, _probe: InlineProbe) {
        reply_who(ctx, INLINE_WHO_PARENT);
    }
}

/// Inline child — co-located in the parent's wasm instance. `Instanced`
/// so it satisfies the `spawn_inline_child` bound; not exported (the parent
/// constructs it in-process).
pub struct InlineDespawnChild;

impl Instanced for InlineDespawnChild {}

#[actor]
impl FfiActor for InlineDespawnChild {
    const NAMESPACE: &'static str = "test.inline.despawn_child";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineDespawnChild)
    }

    /// Answer an `InlineProbe` addressed to the child's alias with the child
    /// marker — the membrane's child-demux path.
    #[handler::manual]
    fn on_probe(&mut self, ctx: &mut FfiCtx<'_, Manual>, _probe: InlineProbe) {
        reply_who(ctx, INLINE_WHO_CHILD);
    }

    /// Self-despawn: tear *itself* down mid-dispatch (ADR-0114 reentrant
    /// teardown). The membrane has this child taken out of its slot while it
    /// runs, so `despawn_inline_child` clears the empty slot and the
    /// membrane's `reinsert` no-ops, dropping the live box at end of
    /// dispatch. The child's own alias is the ctx's mailbox id.
    #[handler::manual]
    fn on_despawn(&mut self, ctx: &mut FfiCtx<'_, Manual>, _trigger: DespawnChild) {
        let _ = ctx.despawn_inline_child(MailboxId(ctx.mailbox_id()));
    }
}

aether_actor::export!(InlineDespawnParent);
