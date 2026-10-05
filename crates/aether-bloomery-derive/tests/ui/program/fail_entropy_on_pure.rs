use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Entropy, Env, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.entropy.pure.input")]
struct In {
    /// A test value.
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.entropy.pure.result")]
struct Out {
    n: u32,
}

struct PureEntropy;

/// A test program.
#[program]
impl Program for PureEntropy {
    const NAME: &'static str = "test.program.entropy.pure";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because Mode::Pure cannot take Entropy.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, _env: &mut Env<Async>, _entropy: Entropy) -> Result<Self::Result, Refusal> {
        Ok(Out { n: input.n })
    }
}

fn main() {}
