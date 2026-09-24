// `type Alias = T` is not followed; generators need a path (or import alias) to the type.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};

pub struct Probe;

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.bloomery.export.ty_alias";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

type Alias = Probe;

export!(public = [Alias], generators = [aether_bloomery_bundle::bundle]);

fn main() {}
