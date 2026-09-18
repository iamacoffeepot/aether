//! Two-program WASM bundle for the program-root `SubstrateHarness` test.

#![allow(clippy::unused_self)]

use aether_actor::export;
use aether_bloomery_kinds::{Mode, Ref, Refusal, Utf8Text};
use aether_bloomery_program::kinds::Detail;
use aether_bloomery_program::{Env, Program, Pure, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.result")]
struct SummarizeResult {
    text: Ref<Utf8Text>,
}

#[allow(dead_code)]
struct Summarize;

#[program]
impl Program for Summarize {
    const NAME: &'static str = "test.program.summarize";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read cited text and stage a derived summary.";
    type Input = SummarizeInput;
    type Result = SummarizeResult;

    fn run(input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        let text = env.read_text(input.text)?;
        Ok(SummarizeResult { text: env.stage_text(&format!("summary:{text}")) })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.refuse.input")]
struct RefuseInput {
    marker: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.refuse.result")]
struct RefuseResult {
    marker: u32,
}

#[allow(dead_code)]
struct Refuse;

#[program]
impl Program for Refuse {
    const NAME: &'static str = "test.program.refuse";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Always refuse the invocation.";
    type Input = RefuseInput;
    type Result = RefuseResult;

    fn run(_input: Self::Input, _env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        Err(Refusal::Refused { reason: Detail::new("refused") })
    }
}

export!(Summarize, Refuse, generators = [aether_bloomery_program::bundle_programs]);
