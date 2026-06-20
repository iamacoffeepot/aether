//! Baseline: a minimal well-formed `#[actor] impl WasmActor` — one
//! `#[handler]` plus a `const NAMESPACE` — expands cleanly. Guards the
//! reject fixtures against false positives: the new diagnostics must
//! fire on the malformed shapes, not on every actor.

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

struct Minimal;

#[actor]
impl aether_actor::WasmActor for Minimal {
    const NAMESPACE: &'static str = "minimal";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(Minimal)
    }

    #[handler]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
