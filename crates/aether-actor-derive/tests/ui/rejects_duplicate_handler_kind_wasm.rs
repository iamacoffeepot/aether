//! Two `#[handler]` methods on one `#[actor] impl WasmActor` block that
//! accept the same mail kind are rejected, spanned at the later handler.
//! Each kind routes to exactly one handler; a duplicate would emit two
//! `HandlesKind<K>` impls (a coherence error) and a dead second dispatch
//! arm.

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
struct Dup;

#[actor]
impl aether_actor::WasmActor for Dup {
    const NAMESPACE: &'static str = "dup";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(Dup)
    }

    #[handler]
    fn on_first(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}

    #[handler]
    fn on_second(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
