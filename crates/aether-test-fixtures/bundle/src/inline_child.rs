//! `inline_child` bundle — the ADR-0114 inline-child fixtures for the
//! basic, stateful, and despawn scenarios, exported together via
//! `export!(InlineParent, InlineStatefulParent, InlineStatefulChild,
//! InlineDespawnParent)` (ADR-0096, issue 1994).
//!
//! # `InlineParent` (entry)
//!
//! ADR-0114 inline-child fixture (#1916). The entry `InlineParent`
//! spawns a co-located `InlineChild` in `wire` via
//! `ctx.spawn_inline_child::<InlineChild>` (ADR-0114). The child gets a
//! first-class lineage address (`{parent}/aether.embedded:widget`) routed
//! to the parent's one slot; the `export!` membrane demuxes mail
//! addressed to that alias to the child.
//!
//! Both actors answer the same `InlineProbe` query with an `InlineEcho`
//! tagged by `who` handled it, so a `FleetBench` scenario can send to the
//! child's address over the real wire and assert the *child* (not the
//! parent) replied. `InlineChild` is `Instanced` (the `spawn_inline_child`
//! bound); it is not in the `export!` list because an inline child is
//! constructed in-process by the parent, not instantiated by the host.
//!
//! # `InlineStatefulParent` / `InlineStatefulChild`
//!
//! ADR-0114 §5 fixture (#1930): a multi-actor module whose entry
//! `InlineStatefulParent` spawns a co-located, stateful
//! `InlineStatefulChild` in `wire` via `ctx.spawn_inline_child`. The
//! child declares `type State = InlineCounterState` (ADR-0113), so the
//! `#[actor]` macro generates its hot-swap hooks; both types are in the
//! `export!` list so the rehydrate path can reconstruct the child by type
//! after a `replace_component` swap.
//!
//! Consumers load this actor from the `inline_child` bundle with
//! `export: Some("test.inline.stateful_parent")`.
//!
//! # `InlineDespawnParent`
//!
//! ADR-0114 inline-child teardown fixture (#1939). The entry
//! `InlineDespawnParent` spawns a co-located `InlineDespawnChild` in
//! `wire` and stores the returned alias, so a `DespawnChild` trigger to
//! the parent tears the child down via `ctx.despawn_inline_child`.
//!
//! Consumers load this actor from the `inline_child` bundle with
//! `export: Some("test.inline.despawn_parent")`.

// `#[handler::manual]` methods take `&mut self` to match the dispatch ABI
// even though stateless replies never read it. `rehydrate` takes its
// `State` by value; `InlineCounterState` is all-`Copy`, so clippy reads
// the by-value parameter as needlessly owned — the contract is the point.
#![allow(clippy::unused_self, clippy::needless_pass_by_value)]

use aether_actor::{
    BootError, Mail, Manual, OutboundReply, Subname, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_data::MailboxId;
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DespawnChild, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho,
    InlineProbe,
};

/// Durable state the `InlineStatefulChild` carries across `replace_component`.
/// Uses the `aether.test_fixtures.inline_counter_state` shape so the macro
/// frames it via `save_state_kind` on dehydrate and recovers it via
/// `as_kind` on rehydrate.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.test_fixtures.inline_counter_state")]
pub struct InlineCounterState {
    pub count: u32,
}

/// Reply to an `InlineProbe` with the `who` marker of whichever actor
/// handled it — shared by the basic and despawn parent/child actors.
fn reply_who(ctx: &mut WasmCtx<'_, Manual>, who: u32) {
    if ctx.reply_target().is_some() {
        ctx.reply(&InlineEcho { who });
    }
}

/// Entry export — the basic ADR-0114 #1916 fixture. Spawns its inline
/// child in `wire`.
pub struct InlineParent;

#[actor]
impl WasmActor for InlineParent {
    const NAMESPACE: &'static str = "test.inline.parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineParent)
    }

    /// ADR-0114: co-locate an `InlineChild` under the `Named` subname
    /// `widget`. The returned alias `MailboxId` is fire-and-forget here —
    /// the `FleetBench` addresses the child by its rendered lineage name.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        let _ = ctx.spawn_inline_child::<InlineChild>(Subname::Named("widget"), &());
    }

    /// Answer an `InlineProbe` addressed to the parent's own mailbox with
    /// the parent marker — the membrane's own-id (control) path.
    #[handler::manual]
    fn on_probe(&mut self, ctx: &mut WasmCtx<'_, Manual>, _probe: InlineProbe) {
        reply_who(ctx, INLINE_WHO_PARENT);
    }
}

/// Inline child for the basic `InlineParent` fixture. `Instanced` so it
/// satisfies the `spawn_inline_child` bound; not exported (the parent
/// constructs it in-process).
pub struct InlineChild;

#[actor(instanced)]
impl WasmActor for InlineChild {
    const NAMESPACE: &'static str = "test.inline.child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineChild)
    }

    /// Answer an `InlineProbe` addressed to the child's alias with the
    /// child marker — the membrane's child-demux path.
    #[handler::manual]
    fn on_probe(&mut self, ctx: &mut WasmCtx<'_, Manual>, _probe: InlineProbe) {
        reply_who(ctx, INLINE_WHO_CHILD);
    }
}

/// Entry export for the ADR-0114 §5 #1930 stateful-child fixture. Spawns
/// a stateful `InlineStatefulChild` in `wire` and otherwise ignores mail.
///
/// Load from the `inline_child` bundle with
/// `export: Some("test.inline.stateful_parent")`.
pub struct InlineStatefulParent;

#[actor]
impl WasmActor for InlineStatefulParent {
    const NAMESPACE: &'static str = "test.inline.stateful_parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineStatefulParent)
    }

    /// ADR-0114: co-locate an `InlineStatefulChild` under the `Named`
    /// subname `widget`. The child is addressed by its rendered lineage
    /// name (`{parent}/aether.embedded:widget`); the membrane demuxes
    /// the `Bump` / `CountQuery` mail to it.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        let _ = ctx.spawn_inline_child::<InlineStatefulChild>(Subname::Named("widget"), &());
    }

    /// The parent ignores mail addressed to its own id — only the child
    /// carries state. A `#[fallback]` keeps the parent a valid receiver.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

/// Inline child for the stateful fixture, co-located in the parent's wasm
/// instance, carrying a counter that survives a `replace_component` swap.
/// `Instanced` satisfies the `spawn_inline_child` bound; it is in the
/// `export!` list so the rehydrate reconstruct can re-`init` it by type.
pub struct InlineStatefulChild {
    count: u32,
}

#[actor(instanced)]
impl WasmActor for InlineStatefulChild {
    const NAMESPACE: &'static str = "test.inline.stateful_child";

    /// ADR-0113: the durable shape. The `#[actor]` macro generates the
    /// child's `on_dehydrate` / `on_rehydrate` from this plus the
    /// accessors below, and ADR-0114 §5 packs / restores them through
    /// the composite migration bundle.
    type State = InlineCounterState;

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineStatefulChild { count: 0 })
    }

    /// Save-side accessor: snapshot the live counter for the composite.
    fn dehydrate(&self) -> InlineCounterState {
        InlineCounterState { count: self.count }
    }

    /// Restore-side accessor: adopt the recovered snapshot after the swap.
    fn rehydrate(&mut self, state: InlineCounterState) {
        self.count = state.count;
    }

    /// Increment the child's in-memory counter (mail demuxed to the
    /// child's alias).
    #[handler]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    /// Reply with the live counter so a test can read the child's state
    /// across a swap.
    #[handler::manual]
    fn on_count_query(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: CountQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&CountReport { count: self.count });
        }
    }
}

/// Entry export for the ADR-0114 #1939 teardown fixture. Spawns an
/// `InlineDespawnChild` in `wire`, stores the alias, and tears the child
/// down on a `DespawnChild` trigger.
///
/// Load from the `inline_child` bundle with
/// `export: Some("test.inline.despawn_parent")`.
pub struct InlineDespawnParent {
    /// The spawned child's alias `MailboxId` (set in `wire`), the handle
    /// the `DespawnChild` handler tears down. `None` until `wire` runs.
    child: Option<MailboxId>,
}

#[actor]
impl WasmActor for InlineDespawnParent {
    const NAMESPACE: &'static str = "test.inline.despawn_parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineDespawnParent { child: None })
    }

    /// ADR-0114: co-locate an `InlineDespawnChild` under the `Named` subname
    /// `widget` and store the returned alias so the `DespawnChild` handler
    /// can tear it down.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
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
    fn on_despawn(&mut self, ctx: &mut WasmCtx<'_, Manual>, _trigger: DespawnChild) {
        if let Some(child) = self.child {
            let _ = ctx.despawn_inline_child(child);
        }
    }

    /// Answer an `InlineProbe` addressed to the parent's own mailbox with
    /// the parent marker — the membrane's own-id (control) path, and the
    /// post-teardown fallthrough target for a probe to the dead child alias.
    #[handler::manual]
    fn on_probe(&mut self, ctx: &mut WasmCtx<'_, Manual>, _probe: InlineProbe) {
        reply_who(ctx, INLINE_WHO_PARENT);
    }
}

/// Inline child for the despawn fixture, co-located in the parent's wasm
/// instance. `Instanced` so it satisfies the `spawn_inline_child` bound;
/// not exported (the parent constructs it in-process).
pub struct InlineDespawnChild;

#[actor(instanced)]
impl WasmActor for InlineDespawnChild {
    const NAMESPACE: &'static str = "test.inline.despawn_child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineDespawnChild)
    }

    /// Answer an `InlineProbe` addressed to the child's alias with the
    /// child marker — the membrane's child-demux path.
    #[handler::manual]
    fn on_probe(&mut self, ctx: &mut WasmCtx<'_, Manual>, _probe: InlineProbe) {
        reply_who(ctx, INLINE_WHO_CHILD);
    }

    /// Self-despawn: tear *itself* down mid-dispatch (ADR-0114 reentrant
    /// teardown). The child's own alias is the ctx's mailbox id.
    #[handler::manual]
    fn on_despawn(&mut self, ctx: &mut WasmCtx<'_, Manual>, _trigger: DespawnChild) {
        let _ = ctx.despawn_inline_child(MailboxId(ctx.mailbox_id()));
    }
}
