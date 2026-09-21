// Catches `#[program]` accepting an invalid NAME, which then fails only when the driver decodes the bundle.
use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.invalid.name.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.invalid.name.result")]
struct Out {
    n: u32,
}

struct InvalidNameProg;

#[program]
impl Program for InvalidNameProg {
    const NAME: &'static str = "Test.Program";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because NAME is not a valid ProgramName.";
    type Input = In;
    type Result = Out;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(input)
    }
}

fn main() {}
