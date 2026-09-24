use aether_actor::export;
use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.pass.one.input")]
struct OneIn {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.pass.one.result")]
struct OneOut {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.pass.two.input")]
struct TwoIn {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.pass.two.result")]
struct TwoOut {
    n: u32,
}

struct One;

#[program]
impl Program for One {
    const NAME: &'static str = "test.program.one";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "First program in a two-program bundle.";
    type Input = OneIn;
    type Result = OneOut;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(OneOut { n: input.n })
    }
}

struct Two;

#[program]
impl Program for Two {
    const NAME: &'static str = "test.program.two";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Second program in a two-program bundle.";
    type Input = TwoIn;
    type Result = TwoOut;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(TwoOut { n: input.n })
    }
}

export!(public = [One, Two], generators = [aether_bloomery_bundle::bundle]);

fn main() {
    let _ = aether_bloomery_bundle::BUNDLE_NAMESPACE;
}
