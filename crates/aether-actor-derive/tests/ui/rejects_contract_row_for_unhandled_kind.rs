//! ADR-0231 §1: `#[actor]` emits a `Contract<K>` row per handler and none for a
//! `#[fallback]`, so a kind that only the fallback would catch has no row and
//! a bound on it names the missing kind.

use aether_actor::{Contract, Kind, WasmCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.contract_row.handled")]
struct Handled {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.contract_row.unhandled")]
struct Unhandled {
    seq: u32,
}

struct FallbackProbe;

#[actor]
impl aether_actor::WasmActor for FallbackProbe {
    const NAMESPACE: &'static str = "contract_row_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_handled(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Handled) {}

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

fn assert_row<T: Contract<K>, K: Kind>() {}

fn main() {
    assert_row::<FallbackProbe, Handled>();
    assert_row::<FallbackProbe, Unhandled>();
}
