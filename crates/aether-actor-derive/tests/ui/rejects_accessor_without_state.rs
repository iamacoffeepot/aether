//! ADR-0113: a `dehydrate` / `rehydrate` accessor with no `type State`
//! declaration has no kind to (de)serialize — the accessor pair only
//! means anything alongside a declared state kind. Rejected at the
//! accessor's span.

use aether_actor::actor;
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
    fn on_bump(&mut self, _ctx: &mut aether_actor::FfiCtx<'_>, bump: Bump) {
        self.count += bump.delta;
    }
}

fn main() {}
