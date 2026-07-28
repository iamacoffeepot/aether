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
//! tagged by `who` handled it, so a `FleetHarness` scenario can send to the
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
//!
//! # `InlineConfiguredParent` / `InlineConfiguredChild`
//!
//! Issue 2690 fixture: the coverage hole every other inline-child fixture
//! here leaves open. `InlineStatefulChild` above carries durable state
//! but spawns with a `()` config, which decodes `Some(())` from empty
//! bytes on reconstruct — the branch that never exercises a typed
//! (non-`()`) config's decode-from-real-bytes path. The entry
//! `InlineConfiguredParent` spawns a co-located `InlineConfiguredChild`
//! in `wire` with a **non-default** `InlineConfiguredChildConfig`
//! (`initial: CONFIGURED_CHILD_INITIAL`), and the child's durable
//! `InlineCounterState` starts from that config value rather than `0` —
//! so a reload that silently re-inited from a default/empty config would
//! be distinguishable from one that decoded the real bytes.
//!
//! Consumers load this actor from the `inline_child` bundle with
//! `export: Some("test.inline.configured_parent")`.
//!
//! # `InlineTagParent`
//!
//! Issue 2692 by-tag inline-spawn fixture. The entry `InlineTagParent`
//! spawns the (already-exported) `InlineStatefulChild` in `wire` by
//! *runtime tag* — `ctx.spawn_inline_child_by_tag(ActorTypeTag::of::<
//! InlineStatefulChild>(), …)` — rather than the compile-time-typed verb,
//! exercising the real `export!`-generated resolver. It also attempts a
//! deliberately-unknown-tag spawn and records whether the resolver returned
//! `UnknownActorTag`, surfaced on a `TagSpawnQuery`. Because the tagged
//! child is `InlineStatefulChild`, its state reconstructs across a
//! `replace_component` swap through the same reconstruct arm the tag came
//! from.
//!
//! Consumers load this actor from the `inline_child` bundle with
//! `export: Some("test.inline.tag_parent")`.

// `#[handler::manual]` methods take `&mut self` to match the dispatch ABI
// even though stateless replies never read it. `rehydrate` takes its
// `State` by value; `InlineCounterState` is all-`Copy`, so clippy reads
// the by-value parameter as needlessly owned — the contract is the point.
#![allow(clippy::unused_self, clippy::needless_pass_by_value)]

use aether_actor::{
    ActorInitError, ActorTypeTag, Mail, Manual, OutboundReply, SpawnError, Subname, WasmActor, WasmCtx, WasmInitCtx,
    actor,
};
use aether_data::MailboxId;
use aether_test_fixtures_kinds::{
    Bump, CONFIGURED_CHILD_INITIAL, CountQuery, CountReport, DespawnChild, INLINE_WHO_CHILD, INLINE_WHO_PARENT,
    InlineConfiguredChildConfig, InlineEcho, InlineProbe, TagSpawnQuery, TagSpawnReport,
};

/// Durable state the `InlineStatefulChild` carries across `replace_component`.
/// Uses the `aether.test_fixtures.inline_counter_state` shape so the macro
/// frames it via `save_state_kind` on dehydrate and recovers it via
/// `decode_kind` on rehydrate.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
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

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InlineParent)
    }

    /// ADR-0114: co-locate an `InlineChild` under the `Named` subname
    /// `widget`. The returned alias `MailboxId` is fire-and-forget here —
    /// the `FleetHarness` addresses the child by its rendered lineage name.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
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

#[actor(instanced, child_of(InlineParent))]
impl WasmActor for InlineChild {
    const NAMESPACE: &'static str = "test.inline.child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
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

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InlineStatefulParent)
    }

    /// ADR-0114: co-locate an `InlineStatefulChild` under the `Named`
    /// subname `widget`. The child is addressed by its rendered lineage
    /// name (`{parent}/aether.embedded:widget`); the membrane demuxes
    /// the `Bump` / `CountQuery` mail to it.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        let _ = ctx.spawn_inline_child::<InlineStatefulChild>(Subname::Named("widget"), &());
    }

    /// The parent ignores mail addressed to its own id — only the child
    /// carries state. A `#[fallback]` keeps the parent a valid receiver.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

/// Inline child for the stateful fixture, co-located in the parent's wasm
/// instance, carrying a counter that survives a `replace_component` swap.
/// It is composable because both `InlineStatefulParent` and `InlineTagParent`
/// spawn this exported identity from the same resident module.
pub struct InlineStatefulChild {
    count: u32,
}

#[actor(instanced, composable)]
impl WasmActor for InlineStatefulChild {
    const NAMESPACE: &'static str = "test.inline.stateful_child";

    /// ADR-0113: the durable shape. The `#[actor]` macro generates the
    /// child's `on_dehydrate` / `on_rehydrate` from this plus the
    /// accessors below, and ADR-0114 §5 packs / restores them through
    /// the composite migration bundle.
    type State = InlineCounterState;

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
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
    #[handler::single]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    /// Reply with the live counter so a test can read the child's state
    /// across a swap.
    //noinspection DuplicatedCode -- actor macros require one query handler per hot-swap fixture type.
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

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InlineDespawnParent { child: None })
    }

    /// ADR-0114: co-locate an `InlineDespawnChild` under the `Named` subname
    /// `widget` and store the returned alias so the `DespawnChild` handler
    /// can tear it down.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        if let Ok(alias) = ctx.spawn_inline_child::<InlineDespawnChild>(Subname::Named("widget"), &()) {
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

#[actor(instanced, child_of(InlineDespawnParent))]
impl WasmActor for InlineDespawnChild {
    const NAMESPACE: &'static str = "test.inline.despawn_child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
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
        let _ = ctx.despawn_inline_child(ctx.mailbox_id());
    }
}

/// Entry export for issue 2690's config-carrying inline-child reload
/// fixture. Spawns a co-located `InlineConfiguredChild` in `wire` with a
/// non-default typed config.
///
/// Load from the `inline_child` bundle with
/// `export: Some("test.inline.configured_parent")`.
pub struct InlineConfiguredParent;

#[actor]
impl WasmActor for InlineConfiguredParent {
    const NAMESPACE: &'static str = "test.inline.configured_parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InlineConfiguredParent)
    }

    /// ADR-0114: co-locate an `InlineConfiguredChild` under the `Named`
    /// subname `widget`, spawned with a non-default config so the child's
    /// durable counter starts from `CONFIGURED_CHILD_INITIAL`, not `0`.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        let _ = ctx.spawn_inline_child::<InlineConfiguredChild>(
            Subname::Named("widget"),
            &InlineConfiguredChildConfig { initial: CONFIGURED_CHILD_INITIAL },
        );
    }

    /// The parent ignores mail addressed to its own id — only the child
    /// carries state. A `#[fallback]` keeps the parent a valid receiver.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

/// Config-carrying inline child for issue 2690's reload fixture. `init`
/// seeds the durable counter from the config's `initial` rather than a
/// hardcoded `0`, so a test can move the state off *that* config-derived
/// value and assert the moved value — not the config default — survives
/// a `replace_component` swap. `Instanced` satisfies the
/// `spawn_inline_child` bound; it is in the `export!` list so the
/// rehydrate reconstruct can re-`init` it by type, decoding its real
/// config bytes (the branch issue 2690 fixes).
pub struct InlineConfiguredChild {
    count: u32,
}

#[actor(instanced, child_of(InlineConfiguredParent))]
impl WasmActor for InlineConfiguredChild {
    type Config = InlineConfiguredChildConfig;
    const NAMESPACE: &'static str = "test.inline.configured_child";

    /// ADR-0113: the durable shape, shared with `InlineStatefulChild` —
    /// both carry a plain counter, packed / restored through the
    /// composite migration bundle (ADR-0114 §5).
    type State = InlineCounterState;

    fn init(config: InlineConfiguredChildConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InlineConfiguredChild { count: config.initial })
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
    #[handler::single]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    /// Reply with the live counter so a test can read the child's state
    /// across a swap.
    //noinspection DuplicatedCode -- actor macros require one query handler per hot-swap fixture type.
    #[handler::manual]
    fn on_count_query(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: CountQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&CountReport { count: self.count });
        }
    }
}

/// Entry export for the issue 2692 by-tag inline-spawn fixture. Spawns the
/// exported `InlineStatefulChild` by runtime [`ActorTypeTag`] (not the typed
/// verb) in `wire`, storing the alias, and records whether a bogus tag was
/// correctly rejected.
///
/// Load from the `inline_child` bundle with
/// `export: Some("test.inline.tag_parent")`.
pub struct InlineTagParent {
    /// The by-tag-spawned child's alias `MailboxId` (set in `wire`).
    child: Option<MailboxId>,
    /// Whether the deliberately-unknown-tag spawn attempted in `wire`
    /// returned [`SpawnError::UnknownActorTag`] — the only correct outcome.
    unknown_tag_rejected: bool,
}

#[actor]
impl WasmActor for InlineTagParent {
    const NAMESPACE: &'static str = "test.inline.tag_parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InlineTagParent { child: None, unknown_tag_rejected: false })
    }

    /// Issue 2692: spawn `InlineStatefulChild` **by tag** — the tag resolves
    /// against the module's `export!` set (which includes that child), so no
    /// generic parameter names the child type here. Then attempt a
    /// deliberately-bogus tag and record that the generated resolver rejects
    /// it with `UnknownActorTag` rather than spawning or panicking.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        if let Ok(alias) =
            ctx.spawn_inline_child_by_tag(ActorTypeTag::of::<InlineStatefulChild>(), Subname::Named("tagged"), &[])
        {
            self.child = Some(alias);
        }
        let bogus = ActorTypeTag(0xFFFF_FFFF_FFFF_FFFF);
        self.unknown_tag_rejected = matches!(
            ctx.spawn_inline_child_by_tag(bogus, Subname::Named("nope"), &[]),
            Err(SpawnError::UnknownActorTag(_)),
        );
    }

    /// Report whether the bogus-tag spawn was correctly rejected — the
    /// over-the-wire observable for the generated resolver's `UnknownActorTag`
    /// fall-through.
    #[handler::manual]
    fn on_tag_query(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: TagSpawnQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&TagSpawnReport { unknown_tag_rejected: self.unknown_tag_rejected });
        }
    }

    /// A `#[fallback]` keeps the parent a valid receiver — mail addressed to
    /// its own id that it doesn't handle (the tagged child answers on its own
    /// alias) is absorbed.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}
