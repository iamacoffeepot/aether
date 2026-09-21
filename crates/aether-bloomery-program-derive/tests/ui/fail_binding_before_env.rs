use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Http, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.http.before.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.http.before.result")]
struct Out {
    n: u32,
}

struct BeforeEnv;

#[program]
impl Program for BeforeEnv {
    const NAME: &'static str = "test.program.http.before";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Fails because Http comes before env.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, _http: Http, _env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        Ok(Out { n: input.n })
    }
}

fn main() {}
