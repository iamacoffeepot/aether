//! ADR-0113 decode-miss companion to `stateful_replace_typed.rs`: same
//! `NAMESPACE` and the same `CounterState` kind name, but the state shape
//! gains a `generation` field. Reshaping the schema changes `Kind::ID`,
//! so when this wasm replaces the typed fixture the generated
//! `on_rehydrate` sees `PriorState::as_kind::<CounterState>() == None`
//! (the saved bundle carries the old id), warns, and boots fresh — the
//! counter resets to its `init` zero instead of restoring.

// See `stateful_replace_typed.rs`: `rehydrate` takes its `State` by value
// (the by-value persistence contract); clippy reads that as needlessly
// owned for an all-`Copy` state, so silence the false positive.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::{BootError, FfiActor, FfiCtx, Manual, OutboundReply, Resolver, actor};
use aether_test_fixtures::{Bump, CountQuery, CountReport};

/// Reshaped durable state — the added `generation` field changes the
/// schema and therefore `Kind::ID`, which is what drives the decode-miss
/// when this fixture replaces `stateful_replace_typed`.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.test_fixtures.counter_state")]
pub struct CounterState {
    pub count: u32,
    pub generation: u32,
}

/// The evolved counter, tracking the new `generation` field its reshaped
/// state carries. The added field is what makes this a genuine schema
/// evolution: when this replaces the typed fixture, the saved bundle's
/// `Kind::ID` no longer matches and rehydrate misses.
pub struct Counter {
    count: u32,
    generation: u32,
}

#[actor]
impl FfiActor for Counter {
    const NAMESPACE: &'static str = "stateful.typed";

    type State = CounterState;

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(Counter {
            count: 0,
            generation: 0,
        })
    }

    fn dehydrate(&self) -> CounterState {
        CounterState {
            count: self.count,
            generation: self.generation,
        }
    }

    fn rehydrate(&mut self, state: CounterState) {
        self.count = state.count;
        self.generation = state.generation;
    }

    #[handler]
    fn on_bump(&mut self, _ctx: &mut FfiCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    #[handler::manual]
    fn on_count_query(&mut self, ctx: &mut FfiCtx<'_, Manual>, _query: CountQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&CountReport { count: self.count });
        }
    }
}

aether_actor::export!(Counter);
