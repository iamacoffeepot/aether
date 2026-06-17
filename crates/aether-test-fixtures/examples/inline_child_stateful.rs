//! ADR-0114 §5 fixture: a multi-actor module whose entry
//! `InlineStatefulParent` spawns a co-located, **stateful**
//! `InlineStatefulChild` in `wire` via `ctx.spawn_inline_child`. The
//! child declares `type State = CounterState` (ADR-0113), so the
//! `#[actor]` macro generates its hot-swap hooks; the parent + child are
//! both in the `export!` list so the rehydrate path can reconstruct the
//! child **by type** after a `replace_component` swap.
//!
//! The child answers `Bump` by incrementing its counter and `CountQuery`
//! by replying the live count, so a `TestBench` scenario can mutate the
//! child's state, hot-reload the same wasm, then re-query the child's
//! alias and assert the prior count survived the swap — the round-trip
//! ADR-0114 §5 (dehydrate → composite bundle → rehydrate reconstruct)
//! delivers.
//!
//! `InlineStatefulChild` rides the `export!` list (unlike the stateless
//! `inline_child` fixture's child) because the rehydrate reconstruct
//! resolves the child's type tag against the module's exported type set
//! (ADR-0096) — a co-located child type must be exported to be
//! reconstructable.

// The parent's `#[fallback]` and the child's `#[handler::manual]` take
// `&mut self` to match the dispatch ABI even when an arm is stateless.
#![allow(clippy::unused_self)]
// `rehydrate` takes its `State` by value (the macro moves the decoded
// state into the actor); `CounterState` is all-`Copy`, so clippy reads the
// by-value parameter as needlessly owned — the contract is the point.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::{
    BootError, FfiActor, FfiCtx, FfiInitCtx, Instanced, Mail, Manual, OutboundReply, Subname, actor,
};
use aether_test_fixtures::{Bump, CountQuery, CountReport};

/// Durable state the inline child carries across `replace_component`.
/// Reuses the `aether.test_fixtures.counter_state` shape so the macro
/// frames it via `save_state_kind` on dehydrate and recovers it via
/// `as_kind` on rehydrate.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.test_fixtures.inline_counter_state")]
pub struct InlineCounterState {
    pub count: u32,
}

/// Entry export — the loaded component. Spawns its stateful inline child
/// in `wire` and otherwise ignores mail.
pub struct InlineStatefulParent;

#[actor]
impl FfiActor for InlineStatefulParent {
    const NAMESPACE: &'static str = "test.inline.stateful_parent";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(InlineStatefulParent)
    }

    /// ADR-0114: co-locate an `InlineStatefulChild` under the `Named`
    /// subname `widget`. The child is addressed by its rendered lineage
    /// name (`{parent}/aether.embedded:widget`); the membrane demuxes the
    /// `Bump` / `CountQuery` mail to it.
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        let _ = ctx.spawn_inline_child::<InlineStatefulChild>(Subname::Named("widget"), &());
    }

    /// The parent ignores mail addressed to its own id — only the child
    /// carries state. A `#[fallback]` keeps the parent a valid receiver.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut FfiCtx<'_>, _mail: Mail<'_>) {}
}

/// Inline child — co-located in the parent's wasm instance, carrying a
/// counter that must survive a `replace_component` swap. `Instanced`
/// satisfies the `spawn_inline_child` bound; it is in the `export!` list
/// so the rehydrate reconstruct can re-`init` it by type.
pub struct InlineStatefulChild {
    count: u32,
}

impl Instanced for InlineStatefulChild {}

#[actor]
impl FfiActor for InlineStatefulChild {
    const NAMESPACE: &'static str = "test.inline.stateful_child";

    /// ADR-0113: the durable shape. The `#[actor]` macro generates the
    /// child's `on_dehydrate` / `on_rehydrate` from this plus the
    /// accessors below, and ADR-0114 §5 packs / restores them through the
    /// composite migration bundle.
    type State = InlineCounterState;

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
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
    fn on_bump(&mut self, _ctx: &mut FfiCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    /// Reply with the live counter so a test can read the child's state
    /// across a swap.
    #[handler::manual]
    fn on_count_query(&mut self, ctx: &mut FfiCtx<'_, Manual>, _query: CountQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&CountReport { count: self.count });
        }
    }
}

aether_actor::export!(InlineStatefulParent, InlineStatefulChild);
