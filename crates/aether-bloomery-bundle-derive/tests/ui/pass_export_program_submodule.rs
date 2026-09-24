// A program in a submodule, whose input and result types are not in scope at
// the crate root and whose signature names `Refusal` by a plain import, exports
// from the crate root with no path spelled out and no unused import. The types
// are `pub(crate)` only because an impl's associated types may not be less
// visible than the program type `export!` names.

#![deny(unused_imports)]

use aether_actor::export;

mod turns {
    use aether_bloomery_kinds::{Mode, Refusal};
    use aether_bloomery_program::{Async, Env, Http, Program, program};

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.program.ui.submodule.turn.input")]
    pub(crate) struct In {
        n: u32,
    }

    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "test.program.ui.submodule.turn.result")]
    pub(crate) struct Out {
        n: u32,
    }

    pub(crate) struct Turn;

    #[program]
    impl Program for Turn {
        const NAME: &'static str = "test.program.submodule.turn";
        const MODE: Mode = Mode::Sampled;
        const INTENT: &'static str = "A program whose types resolve only inside its own module.";
        type Input = In;
        type Result = Out;

        async fn run(input: Self::Input, _env: &mut Env<Async>, _http: Http) -> Result<Self::Result, Refusal> {
            Ok(Out { n: input.n })
        }
    }
}

export!(turns::Turn, generators = [aether_bloomery_bundle::bundle]);

fn main() {
    let _ = aether_bloomery_bundle::BUNDLE_NAMESPACE;
}
