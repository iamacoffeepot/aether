//! A const other than `NAMESPACE` inside an `#[actor] impl WasmActor`
//! block is stray — the `Actor` super-trait carries no other authorable
//! const — and is rejected at its own span rather than silently routed
//! onto the sibling `impl Actor` block.

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

#[allow(dead_code)]
struct StrayConst;

#[actor]
impl aether_actor::WasmActor for StrayConst {
    const NAMESPACE: &'static str = "stray";
    const BUFFER_CAPACITY: usize = 64;

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::BootError>
    {
        Ok(StrayConst)
    }

    #[handler]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
