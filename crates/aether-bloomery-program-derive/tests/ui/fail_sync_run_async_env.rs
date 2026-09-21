use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.sync.asyncenv.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.sync.asyncenv.result")]
struct Out {
    n: u32,
}

struct SyncProg;

#[program]
impl Program for SyncProg {
    const NAME: &'static str = "test.program.sync.asyncenv";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because sync run takes Env<Async>.";
    type Input = In;
    type Result = Out;

    fn run(input: Self::Input, _env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        Ok(input)
    }
}

fn main() {}
