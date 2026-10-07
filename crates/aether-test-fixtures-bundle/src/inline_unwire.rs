//! Inline-unwire fixtures (issue #7536, ADR-0249 §4, §5, §6): a three-level
//! inline cluster whose every actor reports its `unwire` to the harness
//! observer, so a test reads the order a close ran them in.
//!
//! - `UnwireParent` (`test.inline.unwire_parent`, root) spawns one
//!   `UnwireChild` at the key `child` in `wire`, and spawns that key again on
//!   `RespawnChild`, answering whether the host refused it.
//! - `UnwireChild` (`test.inline.unwire_child`) counts each `Bump`, spawns one
//!   `UnwireLeaf` at the key `leaf` in its own `wire`, and despawns itself on
//!   `DespawnSelf`.
//! - `UnwireLeaf` (`test.inline.unwire_leaf`) only reports its `unwire`.
//!
//! No actor here holds its child: the parent names the child by key each time
//! it needs it. All three declare the observer, so the parent loads only on
//! the `SubstrateHarness`. The child and the leaf are exported beside the
//! parent, as `InlineStatefulChild` and the nested-lineage children are.

use aether_actor::{ActorInitError, Mail, SpawnError, Subname, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DespawnSelf, InlineChildUnwired, InlineLeafUnwired, InlineParentUnwired,
    RespawnChild, RespawnResult, SubstrateHarnessObserver,
};

/// The key `UnwireParent` spawns its child at.
const CHILD_KEY: &str = "child";

/// The key `UnwireChild` spawns its leaf at.
const LEAF_KEY: &str = "leaf";

pub struct UnwireParent;

#[actor(root, spawns(UnwireChild), depends(SubstrateHarnessObserver))]
impl WasmActor for UnwireParent {
    const NAMESPACE: &'static str = "test.inline.unwire_parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(UnwireParent)
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.spawn_inline::<UnwireChild>(Subname::Named(CHILD_KEY), &())
            .map(drop)
            .map_err(|error| ActorInitError::new(format!("the child does not spawn: {error:?}")))
    }

    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.send::<SubstrateHarnessObserver>(&InlineParentUnwired);
    }

    /// Spawn the child's key again, as a `wire` run a second time does, and
    /// report whether the host refused the alias. The child is standing, so
    /// the spawn answers it and builds nothing.
    #[handler::request]
    fn on_respawn(&mut self, ctx: &mut WasmCtx<'_>, _trigger: RespawnChild) -> RespawnResult {
        let spawned = ctx.spawn_inline::<UnwireChild>(Subname::Named(CHILD_KEY), &());
        RespawnResult { alias_refused: matches!(spawned, Err(SpawnError::AliasAllocationFailed)) }
    }
}

/// Counts each `Bump`, so a test can tell the child that stands from one
/// built in its place.
pub struct UnwireChild {
    count: u32,
}

#[actor(instanced, child_of(UnwireParent), spawns(UnwireLeaf), depends(SubstrateHarnessObserver))]
impl WasmActor for UnwireChild {
    const NAMESPACE: &'static str = "test.inline.unwire_child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(UnwireChild { count: 0 })
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.spawn_inline::<UnwireLeaf>(Subname::Named(LEAF_KEY), &())
            .map(drop)
            .map_err(|error| ActorInitError::new(format!("the leaf does not spawn: {error:?}")))
    }

    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.send::<SubstrateHarnessObserver>(&InlineChildUnwired);
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    #[handler::request]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }

    /// Despawn this child from its own handler. Its `unwire` runs once the
    /// handler has returned.
    #[handler::tell]
    fn on_despawn_self(&mut self, ctx: &mut WasmCtx<'_>, despawn: DespawnSelf) {
        let DespawnSelf { me } = despawn;
        match ctx.resolve_path(&me) {
            Ok(me) => {
                ctx.despawn_inline_child(me);
            }
            Err(error) => {
                tracing::warn!(target: "aether_test_fixture_inline_unwire", ?error, "the child's own path did not resolve");
            }
        }
    }
}

pub struct UnwireLeaf;

#[actor(instanced, child_of(UnwireChild), depends(SubstrateHarnessObserver))]
impl WasmActor for UnwireLeaf {
    const NAMESPACE: &'static str = "test.inline.unwire_leaf";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(UnwireLeaf)
    }

    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.send::<SubstrateHarnessObserver>(&InlineLeafUnwired);
    }

    /// The leaf takes no mail of its own. A `#[fallback]` keeps it a valid
    /// receiver.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}
