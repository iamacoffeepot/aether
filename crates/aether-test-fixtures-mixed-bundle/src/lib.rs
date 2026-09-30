//! Mixed program-and-reactor WASM bundle: one program plus one reactor whose
//! rule calls that program.

use aether_actor::export;
use aether_bloomery_kinds::{CallInput, CallProgram, HeadMoved, Mode, ProgramName, Ref, Refusal, Utf8Text};
use aether_bloomery_program::{Env, Program, Sync, program};
use aether_bloomery_reactor::{Guard, NoViews, reactor};
use aether_bloomery_view::At;
use aether_test_fixtures_kinds::{MIXED_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.mixed.summary")]
struct MixedSummary {
    text: Ref<Utf8Text>,
}

struct Summarize;

/// Read cited text and stage a summary derived from it.
#[program]
impl Program for Summarize {
    const NAME: &'static str = "test.program.summarize";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read cited text and stage a derived summary.";
    type Input = SummarizeInput;
    type Result = MixedSummary;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let text = env.injected_text(input.text)?;
        Ok(MixedSummary { text: env.stage_text(&format!("summary:{text}")) })
    }
}

struct SummarizeName(ProgramName);

impl Guard<HeadMoved<SummarizeInput>> for SummarizeName {
    type Views = NoViews;

    fn resolve(_trigger: &HeadMoved<SummarizeInput>, _at: At, (): ()) -> Option<Self> {
        ProgramName::new(SUMMARIZE_PROGRAM).ok().map(Self)
    }
}

pub struct MixedCaller;

#[reactor]
impl Reactor for MixedCaller {
    const NAMESPACE: &'static str = "test.bloomery.mixed.caller";

    #[rule]
    fn call_summarize(&self, change: HeadMoved<SummarizeInput>, name: SummarizeName) -> CallProgram {
        CallProgram { program: MIXED_BUNDLE, name: name.0, input: CallInput::Stored(change.to().digest()) }
    }
}

export!(public = [Summarize, MixedCaller], generators = [aether_bloomery_bundle::bundle]);

const _: Summarize = Summarize;
