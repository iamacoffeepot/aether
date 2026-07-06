//! ADR-0134: a `#[handler::multi]` answers one dispatch with 0..n
//! `ctx.emit` calls, so a non-`()` return has no reply path — the macro
//! rejects it with a pointed error before any dispatch table is emitted
//! (so the native fixture works without linking the substrate types).

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

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.frame")]
struct Frame {
    n: u32,
}

pub struct MultiCap;

#[actor]
impl aether_substrate::actor::native::NativeActor for MultiCap {
    type Config = ();

    const NAMESPACE: &'static str = "test.multi_return_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_actor::ActorInitError> {
        Ok(MultiCap)
    }

    #[handler::multi]
    fn on_ping(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_, aether_substrate::Multi<Frame>>,
        _ping: Ping,
    ) -> u32 {
        0
    }
}

fn main() {}
