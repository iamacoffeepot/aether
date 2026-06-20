//! ADR-0113: `type State` (plus accessors) and a hand-written
//! `on_dehydrate` / `on_rehydrate` are mutually exclusive — the macro
//! already generates the hook from the accessors, so a manual one is
//! rejected at the hand-written hook's span.

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
impl aether_actor::WasmActor for Counter {
    const NAMESPACE: &'static str = "counter";

    type State = CounterState;

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(Counter { count: 0 })
    }

    fn dehydrate(&self) -> CounterState {
        CounterState { count: self.count }
    }

    fn rehydrate(&mut self, state: CounterState) {
        self.count = state.count;
    }

    fn on_dehydrate(&mut self, _ctx: &mut aether_actor::WasmDropCtx<'_>) {}

    #[handler]
    fn on_bump(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, bump: Bump) {
        self.count += bump.delta;
    }
}

fn main() {}
