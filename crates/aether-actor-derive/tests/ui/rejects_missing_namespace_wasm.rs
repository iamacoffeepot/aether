//! An `#[actor] impl WasmActor` block that omits `const NAMESPACE` is
//! rejected at the type, mirroring the `#[bridge]` path — rather than
//! falling through to a later "no associated const NAMESPACE" error
//! against the surfaceless `Actor` trait.

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
struct NoNamespace;

#[actor]
impl aether_actor::WasmActor for NoNamespace {
    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(NoNamespace)
    }

    #[handler]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
