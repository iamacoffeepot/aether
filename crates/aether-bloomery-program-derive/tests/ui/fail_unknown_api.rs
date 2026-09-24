use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.unknown.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.unknown.result")]
struct Out {
    n: u32,
}

struct Other;

struct UnknownApi;

#[program]
impl Program for UnknownApi {
    const NAME: &'static str = "test.program.unknown";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Fails because a trailing binding is neither Http nor Process.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, _env: &mut Env<Async>, _other: Other) -> Result<Self::Result, Refusal> {
        Ok(Out { n: input.n })
    }
}

fn main() {}
