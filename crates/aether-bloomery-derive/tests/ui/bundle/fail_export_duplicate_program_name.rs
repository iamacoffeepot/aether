use aether_actor::export;
use aether_bloomery_kinds::Mode;
use aether_bloomery_program::{Env, Program, Sync, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.dup.input")]
struct In {
    /// A test value.
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.dup.result")]
struct Out {
    n: u32,
}

struct First;

/// A test program.
#[program]
impl Program for First {
    const NAME: &'static str = "test.program.dup";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "First program with a duplicated name.";
    type Input = In;
    type Result = Out;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, aether_bloomery_program::Refusal> {
        Ok(Out { n: input.n })
    }
}

struct Second;

/// A test program.
#[program]
impl Program for Second {
    const NAME: &'static str = "test.program.dup";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Second program with a duplicated name.";
    type Input = In;
    type Result = Out;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, aether_bloomery_program::Refusal> {
        Ok(Out { n: input.n })
    }
}

export!(public = [First, Second], generators = [aether_bloomery_program::bundle]);

fn main() {}
