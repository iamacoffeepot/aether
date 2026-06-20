//! ADR-0112: `#[handler::stream]` is a reserved class with no emit
//! surface yet — the macro rejects it with a pointed "not yet
//! implemented" error on the wasm expansion path.

use aether_actor::{WasmCtx, Stream, actor};

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

struct StreamProbe;

#[actor]
impl aether_actor::WasmActor for StreamProbe {
    const NAMESPACE: &'static str = "stream_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(StreamProbe)
    }

    #[handler::stream]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_, Stream>, _ping: Ping) {}
}

fn main() {}
