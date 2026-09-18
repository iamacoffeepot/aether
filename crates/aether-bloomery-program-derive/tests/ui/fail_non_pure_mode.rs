use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Pure, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.sampled.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.sampled.result")]
struct Out {
    n: u32,
}

struct SampledProg;

#[program]
impl Program for SampledProg {
    const NAME: &'static str = "test.program.sampled";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Fails because MODE is not Pure.";
    type Input = In;
    type Result = Out;

    fn run(input: Self::Input, _env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        Ok(input)
    }
}

fn main() {}
