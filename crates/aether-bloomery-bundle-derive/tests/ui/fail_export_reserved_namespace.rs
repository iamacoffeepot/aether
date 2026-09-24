use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};
use aether_bloomery_kinds::Mode;
use aether_bloomery_program::{Env, Program, Sync, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.reserved.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.reserved.result")]
struct Out {
    n: u32,
}

pub struct Probe;

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "aether.bloomery.bundle";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

struct Listed;

#[program]
impl Program for Listed {
    const NAME: &'static str = "test.program.reserved";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Listed so bundle has a program to select.";
    type Input = In;
    type Result = Out;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, aether_bloomery_program::Refusal> {
        Ok(Out { n: input.n })
    }
}

export!(public = [Probe, Listed], generators = [aether_bloomery_bundle::bundle]);

fn main() {}
