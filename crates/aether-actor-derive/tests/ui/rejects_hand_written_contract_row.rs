//! ADR-0231 §10: a `Contract<K>` row names its position in the actor's one
//! `Contracts::Rows` list, which `#[actor]` writes from its handlers. A
//! hand-written row for a kind the actor has no handler for names a position
//! that holds another kind's row, so it is refused with `E0277`.

use aether_actor::{ActorInitError, Contract, Here, Silent, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hand_written_contract_row.handled")]
struct Handled {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hand_written_contract_row.unhandled")]
struct Unhandled {
    seq: u32,
}

struct A;

#[actor]
impl WasmActor for A {
    const NAMESPACE: &'static str = "test.hand_written_contract_row.a";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_handled(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Handled) {}
}

impl Contract<Unhandled> for A {
    type Reply = Silent;
    type Index = Here;
}

fn main() {}
