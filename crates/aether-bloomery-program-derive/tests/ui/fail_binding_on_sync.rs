use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Http, Program, Sync, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.http.sync.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.http.sync.result")]
struct Out {
    n: u32,
}

struct SyncHttp;

#[program]
impl Program for SyncHttp {
    const NAME: &'static str = "test.program.http.sync";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because trailing bindings require async fn run.";
    type Input = In;
    type Result = Out;

    fn run(input: Self::Input, _env: &mut Env<Sync>, _http: Http) -> Result<Self::Result, Refusal> {
        Ok(Out { n: input.n })
    }
}

fn main() {}
