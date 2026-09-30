//! #7193: `#[handler::unchecked]` gives up the reply check, so it must say
//! why. A bare `#[handler::unchecked]` and one whose `reason` is blank after
//! trimming are both refused at the attribute.

use aether_actor::actor;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.unchecked_reason.ping")]
struct Ping {
    seq: u32,
}

struct NoReason;

#[actor]
impl aether_actor::WasmActor for NoReason {
    const NAMESPACE: &'static str = "test.unchecked_reason.none";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Self)
    }

    #[handler::unchecked]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_, aether_actor::Erased, aether_actor::Unchecked>, _ping: Ping) {}
}

struct BlankReason;

#[actor]
impl aether_actor::WasmActor for BlankReason {
    const NAMESPACE: &'static str = "test.unchecked_reason.blank";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Self)
    }

    #[handler::unchecked(reason = "  ")]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_, aether_actor::Erased, aether_actor::Unchecked>, _ping: Ping) {}
}

fn main() {}
