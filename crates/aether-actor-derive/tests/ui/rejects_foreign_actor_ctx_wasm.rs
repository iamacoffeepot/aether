//! Issue 6279: a handler that spells a *foreign* actor (`WasmCtx<'_, OtherActor>`)
//! fails to unify — the macro builds the typed ctx only for the actor being
//! dispatched (`Self`), so no dispatch arm produces a foreign-actor ctx.

use aether_actor::{WasmCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

struct OtherActor;

struct ForeignProbe;

#[actor]
impl aether_actor::WasmActor for ForeignProbe {
    const NAMESPACE: &'static str = "foreign_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(ForeignProbe)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_, OtherActor>, _ping: Ping) {}
}

fn main() {
    let _ = OtherActor;
}
