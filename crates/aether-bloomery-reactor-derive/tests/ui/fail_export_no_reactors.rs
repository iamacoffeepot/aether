// bundle_reactors on an all-actor export! is misuse, not a silent no-op.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};

pub struct Probe;

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.bloomery.export.none";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

export!(Probe, generators = [aether_bloomery_reactor::bundle_reactors]);

fn main() {}
