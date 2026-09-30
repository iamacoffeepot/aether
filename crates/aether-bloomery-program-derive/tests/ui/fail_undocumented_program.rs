use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.undocumented_program.input")]
struct In {
    /// How many.
    count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.undocumented_program.result")]
struct Out {
    n: u32,
}

struct UndocumentedProgram;

#[program]
impl Program for UndocumentedProgram {
    const NAME: &'static str = "test.program.undocumented_program";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because the impl has no doc.";
    type Input = In;
    type Result = Out;

    fn run(_input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(Out { n: 0 })
    }
}

fn main() {}
