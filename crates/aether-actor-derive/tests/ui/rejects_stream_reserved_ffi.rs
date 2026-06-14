//! ADR-0112: `#[handler::stream]` is a reserved class with no emit
//! surface yet — the macro rejects it with a pointed "not yet
//! implemented" error on the wasm expansion path.

use aether_actor::{FfiCtx, Stream, actor};

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
impl aether_actor::FfiActor for StreamProbe {
    const NAMESPACE: &'static str = "stream_probe";

    fn init<C>(_ctx: &mut C) -> Result<Self, aether_actor::BootError>
    where
        C: aether_actor::Resolver,
    {
        Ok(StreamProbe)
    }

    #[handler::stream]
    fn on_ping(&mut self, _ctx: &mut FfiCtx<'_, Stream>, _ping: Ping) {}
}

fn main() {}
