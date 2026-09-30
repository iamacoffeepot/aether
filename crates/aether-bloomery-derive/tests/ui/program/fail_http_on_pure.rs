use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Http, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.http.pure.input")]
struct In {
    /// A test value.
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.http.pure.result")]
struct Out {
    n: u32,
}

struct PureHttp;

/// A test program.
#[program]
impl Program for PureHttp {
    const NAME: &'static str = "test.program.http.pure";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because Mode::Pure cannot take Http.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, _env: &mut Env<Async>, _http: Http) -> Result<Self::Result, Refusal> {
        Ok(Out { n: input.n })
    }
}

fn main() {}
