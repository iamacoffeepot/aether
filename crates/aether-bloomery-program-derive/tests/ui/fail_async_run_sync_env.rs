use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.async.syncenv.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.async.syncenv.result")]
struct Out {
    n: u32,
}

struct AsyncProg;

#[program]
impl Program for AsyncProg {
    const NAME: &'static str = "test.program.async.syncenv";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because async run takes Env<Sync>.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(input)
    }
}

fn main() {}
