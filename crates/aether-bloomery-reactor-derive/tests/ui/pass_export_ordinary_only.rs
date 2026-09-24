// Ordinary-only export! without generators must keep matching the existing arms.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};

pub struct Probe;

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.bloomery.export.ordinary_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

pub struct Sink;

#[actor]
impl WasmActor for Sink {
    const NAMESPACE: &'static str = "test.bloomery.export.ordinary_sink";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Sink)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

export!(default = Probe, public = [Sink]);

fn main() {
    let _ = Probe;
    let _ = Sink;
}
