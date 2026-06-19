//! ADR-0113 fixture: a single-actor component whose persistent state is
//! declared as `type State` plus a `dehydrate` / `rehydrate` accessor
//! pair, so the `#[actor]` macro generates the `on_dehydrate` /
//! `on_rehydrate` hooks. The counter survives a `replace_component` swap
//! with the same wasm exactly as the hand-written `stateful_replace`
//! fixture's does — but with no hand-written hooks.
//!
//! `stateful_replace_reshaped.rs` is the decode-miss companion: same
//! `NAMESPACE`, a reshaped `CounterState` (an added field changes
//! `Kind::ID`), so a replacement compiled against it sees `as_kind` =
//! `None` and boots fresh.

// `rehydrate` takes its `State` by value — the macro hands the decoded
// state over to be moved into the actor (so a real `State` with heap
// fields needs no clone). This fixture's `CounterState` is all-`Copy`, so
// clippy reads the by-value parameter as needlessly owned; the by-value
// contract is the point, so silence the false positive here.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::{BootError, FfiActor, FfiCtx, FfiInitCtx, Manual, OutboundReply, actor};
use aether_test_fixtures_kinds::{Bump, CountQuery, CountReport};

/// Durable state the `Counter` carries across `replace_component`. The
/// `count` field is the only thing worth persisting — the macro frames it
/// via `save_state_kind` on dehydrate and recovers it via `as_kind` on
/// rehydrate. The reshaped companion fixture adds a field, changing
/// `Kind::ID` so the recovery misses.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.test_fixtures.counter_state")]
pub struct CounterState {
    pub count: u32,
}

/// Single-actor component holding a counter that must survive a swap.
pub struct Counter {
    count: u32,
}

#[actor]
impl FfiActor for Counter {
    const NAMESPACE: &'static str = "stateful.typed";

    /// ADR-0113: the durable shape. Declaring it plus the accessors below
    /// is enough — `#[actor]` generates the `on_dehydrate` /
    /// `on_rehydrate` hooks.
    type State = CounterState;

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Counter { count: 0 })
    }

    /// Save-side accessor: snapshot the live counter. The generated
    /// `on_dehydrate` calls this immediately before the swap.
    fn dehydrate(&self) -> CounterState {
        CounterState { count: self.count }
    }

    /// Restore-side accessor: adopt the recovered snapshot. The generated
    /// `on_rehydrate` hands the decoded state over by value.
    fn rehydrate(&mut self, state: CounterState) {
        self.count = state.count;
    }

    /// Increment the in-memory counter.
    #[handler]
    fn on_bump(&mut self, _ctx: &mut FfiCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    /// Reply with the live counter so a test can read it across a swap.
    #[handler::manual]
    fn on_count_query(&mut self, ctx: &mut FfiCtx<'_, Manual>, _query: CountQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&CountReport { count: self.count });
        }
    }
}

aether_actor::export!(Counter);
