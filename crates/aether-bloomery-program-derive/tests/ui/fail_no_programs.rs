use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};

pub struct Probe;

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.program.export.none";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

export!(Probe, generators = [aether_bloomery_program::bundle_programs]);

fn main() {}
