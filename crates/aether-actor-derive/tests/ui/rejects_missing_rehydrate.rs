//! ADR-0113: `type State` requires both halves of the accessor pair —
//! a lone `dehydrate` (no `rehydrate`) would leave the generated
//! `on_rehydrate` with no method to call. Rejected at the `type State`
//! span, naming the missing accessor.

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

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::BootError>
    {
        Ok(Counter { count: 0 })
    }

    fn dehydrate(&self) -> CounterState {
        CounterState { count: self.count }
    }

    #[handler]
    fn on_bump(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, bump: Bump) {
        self.count += bump.delta;
    }
}

fn main() {}
