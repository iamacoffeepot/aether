//! #7202: `#[handler::single]` is retired on both runtimes, and the refusal
//! names the four intent words that replace it.

use aether_actor::actor;

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.single_refusal.ping")]
pub struct Ping {
    seq: u32,
}

struct WasmSingle;

#[actor]
impl aether_actor::WasmActor for WasmSingle {
    const NAMESPACE: &'static str = "test.single_refusal.wasm";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(WasmSingle)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

pub struct NativeSingle;

#[actor(singleton, runtime_feature = "native-single")]
impl aether_substrate::actor::native::NativeActor for NativeSingle {
    type State = ();
    type Config = ();

    const NAMESPACE: &'static str = "test.single_refusal.native";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<(), aether_substrate::chassis::error::BootError> {
        Ok(())
    }

    #[handler::single]
    fn on_ping(_state: &mut Self::State, _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>, _ping: Ping) {}
}

fn main() {}
