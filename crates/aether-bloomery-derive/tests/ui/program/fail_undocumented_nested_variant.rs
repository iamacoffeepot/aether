use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, program};

/// How to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
enum Speed {
    /// Quickly.
    Fast,
    Slow,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.undocumented_variant.input")]
struct In {
    /// How fast to go, if it matters.
    speed: Option<Speed>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.undocumented_variant.result")]
struct Out {
    n: u32,
}

struct UndocumentedVariant;

/// Fails: `Speed::Slow`, under an `Option`, has no doc.
#[program]
impl Program for UndocumentedVariant {
    const NAME: &'static str = "test.program.undocumented_variant";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because a nested variant has no doc.";
    type Input = In;
    type Result = Out;

    fn run(_input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(Out { n: 0 })
    }
}

fn main() {}
