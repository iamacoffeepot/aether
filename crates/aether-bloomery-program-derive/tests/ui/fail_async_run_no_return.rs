use aether_bloomery_kinds::Mode;
use aether_bloomery_program::{Async, Env, Program, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.async.noreturn.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.async.noreturn.result")]
struct Out {
    n: u32,
}

struct NoReturn;

#[program]
impl Program for NoReturn {
    const NAME: &'static str = "test.program.async.noreturn";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because async run writes no return type.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, _env: &mut Env<Async>) {
        let _ = Out { n: input.n };
    }
}

fn main() {}
