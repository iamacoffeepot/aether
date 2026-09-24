//! Bundle whose one rule turns a move of a `test.program.summarize.input`
//! head into a `CallProgram` for the summarize program.

use aether_bloomery_kinds::{CallProgram, HeadMoved, ProgramName};
use aether_bloomery_reactor::{Guard, NoViews, reactor};
use aether_test_fixtures_kinds::{SUMMARIZE_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};

struct SummarizeName(ProgramName);

impl Guard<HeadMoved<SummarizeInput>> for SummarizeName {
    type Views = NoViews;

    fn resolve(_trigger: &HeadMoved<SummarizeInput>, (): ()) -> Option<Self> {
        ProgramName::new(SUMMARIZE_PROGRAM).ok().map(Self)
    }
}

pub struct SummarizeCaller;

#[reactor]
impl Reactor for SummarizeCaller {
    const NAMESPACE: &'static str = "test.bloomery.summarize.caller";

    #[rule]
    fn call_summarize(&self, change: HeadMoved<SummarizeInput>, name: SummarizeName) -> CallProgram {
        CallProgram { program: SUMMARIZE_BUNDLE, name: name.0, input: change.to().digest() }
    }
}

aether_actor::export!(public = [SummarizeCaller], generators = [aether_bloomery_bundle::bundle]);
