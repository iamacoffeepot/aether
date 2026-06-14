//! ADR-0112: `#[handler::stream]` is reserved on the native expansion
//! path too — the macro rejects it before any dispatch table is emitted,
//! mirroring the wasm fixture.

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
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

struct StreamProbe;

#[actor]
impl aether_substrate::actor::native::NativeActor for StreamProbe {
    const NAMESPACE: &'static str = "stream_probe";
    type Config = ();

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_actor::BootError> {
        Ok(StreamProbe)
    }

    #[handler::stream]
    fn on_ping(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_, aether_substrate::Stream>,
        _ping: Ping,
    ) {
    }
}

fn main() {}
