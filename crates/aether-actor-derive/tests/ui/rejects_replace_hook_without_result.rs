//! ADR-0249 §1: `on_dehydrate` and `on_rehydrate` return the republish's
//! result, as `wire` returns the birth's. A hand-written replace hook with no
//! return type is refused at the hook with the signature to write, rather
//! than left to fail as a type mismatch inside generated code.

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

struct Saver;

#[actor]
impl aether_actor::WasmActor for Saver {
    const NAMESPACE: &'static str = "replace_hook_without_result_saver";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Saver)
    }

    fn on_dehydrate(&mut self, _ctx: &mut aether_actor::WasmDropCtx<'_>) {}

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

struct Restorer;

#[actor]
impl aether_actor::WasmActor for Restorer {
    const NAMESPACE: &'static str = "replace_hook_without_result_restorer";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Restorer)
    }

    fn on_rehydrate(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _prior: aether_actor::PriorState<'_>) {}

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
