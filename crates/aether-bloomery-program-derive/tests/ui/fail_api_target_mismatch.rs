use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::Http as Process;
use aether_bloomery_program::{Async, Env, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.mismatch.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.mismatch.result")]
struct Out {
    n: u32,
}

struct Mismatch;

#[program]
impl Program for Mismatch {
    const NAME: &'static str = "test.program.mismatch";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Fails because the binding is named Process but is the Http API.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, _env: &mut Env<Async>, _process: Process) -> Result<Self::Result, Refusal> {
        Ok(Out { n: input.n })
    }
}

fn main() {}
