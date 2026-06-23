//! ADR-0101 fixture: a multi-actor module whose entry export carries
//! state across `replace_component` through the `on_dehydrate` /
//! `on_rehydrate` lifecycle hooks. Those hooks are now `WasmActor`
//! defaults (no `Replaceable` subtrait, no `export!(X, replaceable)`
//! flag), so a boxed multi-actor instance preserves state across a swap
//! exactly as a single-actor one does — the case the multi-actor arm
//! used to drop by shipping the hooks as no-ops.
//!
//! `Counter` is the entry export (the first type, loaded by an
//! unmodified host). A `Bump` increments an in-memory counter; a
//! `CountQuery` replies with the live value. It overrides `on_dehydrate`
//! to save the counter and `on_rehydrate` to restore it, so replacing it
//! with the same wasm at the same mailbox id keeps the count instead of
//! resetting to the fresh-`init` zero. `Sidecar` is a second trivial
//! export that makes this a genuine multi-actor module
//! (`export!(Counter, Sidecar)`), exercising the boxed `ErasedWasmActor`
//! hot-swap path.

// `#[handler]` / `#[fallback]` methods take `&mut self` to match the
// dispatch ABI even when stateless.
#![allow(clippy::unused_self)]

use aether_actor::{
    ActorInitError, Mail, Manual, OutboundReply, PriorState, WasmActor, WasmCtx, WasmDropCtx,
    WasmInitCtx, actor,
};
use aether_test_fixtures_kinds::{Bump, CountQuery, CountReport};

/// Entry export — the first type in the `export!` list. Holds a counter
/// that must survive `replace_component`.
pub struct Counter {
    count: u32,
}

#[actor]
impl WasmActor for Counter {
    const NAMESPACE: &'static str = "stateful.counter";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Counter { count: 0 })
    }

    /// Increment the in-memory counter.
    #[handler]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    /// Reply with the live counter so a test can read it across a swap.
    #[handler::manual]
    fn on_count_query(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: CountQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&CountReport { count: self.count });
        }
    }

    /// Save-side hot-swap hook: serialize the live counter so the
    /// replacement instance can pick it up. `CountReport` doubles as the
    /// wire shape of the saved bundle.
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        ctx.save_state_kind::<CountReport>(0, &CountReport { count: self.count });
    }

    /// Restore-side hot-swap hook: recover the counter the predecessor
    /// saved. A fresh load (no prior bundle) never reaches here, so the
    /// counter stays at its `init` zero.
    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        if let Some(saved) = prior.as_kind::<CountReport>() {
            self.count = saved.count;
        }
    }
}

/// Sibling export — present only to make the module genuinely
/// multi-actor, so the swap exercises the boxed `ErasedWasmActor` path.
/// `Instanced` so it mirrors the `multi_actor` precedent; carries a
/// `#[fallback]` so its capability group is observably distinct.
pub struct Sidecar;

#[actor(instanced)]
impl WasmActor for Sidecar {
    const NAMESPACE: &'static str = "stateful.sidecar";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Sidecar)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}
