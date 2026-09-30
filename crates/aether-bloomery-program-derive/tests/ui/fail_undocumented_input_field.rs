use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.undocumented_field.input")]
struct In {
    /// How many.
    count: u32,
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.undocumented_field.result")]
struct Out {
    n: u32,
}

struct UndocumentedField;

/// Fails: `In.label` has no doc.
#[program]
impl Program for UndocumentedField {
    const NAME: &'static str = "test.program.undocumented_field";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because an input field has no doc.";
    type Input = In;
    type Result = Out;

    fn run(_input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(Out { n: 0 })
    }
}

fn main() {}
