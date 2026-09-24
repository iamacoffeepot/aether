use aether_actor::export;
use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Http, Process, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.shared.fetch_one.input")]
struct FetchOneIn {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.shared.fetch_one.result")]
struct FetchOneOut {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.shared.fetch_two.input")]
struct FetchTwoIn {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.shared.fetch_two.result")]
struct FetchTwoOut {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.shared.exec.input")]
struct ExecIn {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.shared.exec.result")]
struct ExecOut {
    n: u32,
}

struct FetchOne;

#[program]
impl Program for FetchOne {
    const NAME: &'static str = "test.program.fetch_one";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "First program sharing Http, so the invocation declares its target once.";
    type Input = FetchOneIn;
    type Result = FetchOneOut;

    async fn run(input: Self::Input, _env: &mut Env<Async>, _http: Http) -> Result<Self::Result, Refusal> {
        Ok(FetchOneOut { n: input.n })
    }
}

struct FetchTwo;

#[program]
impl Program for FetchTwo {
    const NAME: &'static str = "test.program.fetch_two";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Second program sharing Http.";
    type Input = FetchTwoIn;
    type Result = FetchTwoOut;

    async fn run(input: Self::Input, _env: &mut Env<Async>, _http: Http) -> Result<Self::Result, Refusal> {
        Ok(FetchTwoOut { n: input.n })
    }
}

struct Exec;

#[program]
impl Program for Exec {
    const NAME: &'static str = "test.program.exec";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "A second, distinct API, so the invocation declares two concrete targets.";
    type Input = ExecIn;
    type Result = ExecOut;

    async fn run(input: Self::Input, _env: &mut Env<Async>, _process: Process) -> Result<Self::Result, Refusal> {
        Ok(ExecOut { n: input.n })
    }
}

export!(FetchOne, FetchTwo, Exec, generators = [aether_bloomery_bundle::bundle]);

fn main() {
    let _ = aether_bloomery_bundle::BUNDLE_NAMESPACE;
}
