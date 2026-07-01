//! Issue #2460: a wasm `#[handler]`'s first parameter must be a `self`
//! receiver. The validator already accepts both `&self` and `&mut self`
//! (it matches any `FnArg::Receiver`), so the diagnostic names both forms
//! rather than the narrower `&mut self`. A non-`self` typed first param
//! earns the generalized error here.

use aether_actor::{WasmCtx, actor};

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

struct ReceiverProbe;

#[actor]
impl aether_actor::WasmActor for ReceiverProbe {
    const NAMESPACE: &'static str = "receiver_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(ReceiverProbe)
    }

    #[handler]
    fn on_ping(_state: u32, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
