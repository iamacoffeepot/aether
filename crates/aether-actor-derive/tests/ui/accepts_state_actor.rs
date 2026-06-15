//! ADR-0113: a well-formed `type State` plus a `dehydrate` / `rehydrate`
//! accessor pair expands cleanly — `#[actor]` generates the
//! `on_dehydrate` / `on_rehydrate` hooks from the declaration and the
//! lifted accessors. Guards the reject fixtures against false positives:
//! the new diagnostics must fire on the malformed shapes, not on every
//! stateful actor.

use aether_actor::{FfiCtx, actor};
use serde::{Deserialize, Serialize};

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "test.counter_state")]
struct CounterState {
    count: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.bump")]
struct Bump {
    delta: u32,
}

struct Counter {
    count: u32,
}

#[actor]
impl aether_actor::FfiActor for Counter {
    const NAMESPACE: &'static str = "counter";

    type State = CounterState;

    fn init<C>(_ctx: &mut C) -> Result<Self, aether_actor::BootError>
    where
        C: aether_actor::Resolver,
    {
        Ok(Counter { count: 0 })
    }

    fn dehydrate(&self) -> CounterState {
        CounterState { count: self.count }
    }

    fn rehydrate(&mut self, state: CounterState) {
        self.count = state.count;
    }

    #[handler]
    fn on_bump(&mut self, _ctx: &mut FfiCtx<'_>, bump: Bump) {
        self.count += bump.delta;
    }
}

fn main() {}
