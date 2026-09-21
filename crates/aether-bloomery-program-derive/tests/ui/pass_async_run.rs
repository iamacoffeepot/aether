use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.async.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.async.result")]
struct Out {
    n: u32,
}

struct AsyncProg;

#[program]
impl Program for AsyncProg {
    const NAME: &'static str = "test.program.async";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Passes because async run takes Env<Async>.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, _env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        Ok(Out { n: input.n })
    }
}

fn main() {}
