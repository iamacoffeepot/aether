use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Entropy, Env, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.sampled.entropy.input")]
struct In {
    /// A test value.
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.sampled.entropy.result")]
struct Out {
    n: u32,
}

struct SampledEntropy;

/// A test program.
#[program]
impl Program for SampledEntropy {
    const NAME: &'static str = "test.program.sampled.entropy";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Passes because Sampled async run takes Entropy after env.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, env: &mut Env<Async>, mut entropy: Entropy) -> Result<Self::Result, Refusal> {
        let _ = env;
        let _ = entropy.draw(core::num::NonZeroU8::new(4).expect("non-zero")).await?;
        Ok(Out { n: input.n })
    }
}

fn main() {}
