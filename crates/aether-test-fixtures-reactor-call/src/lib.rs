//! Bundle whose one rule turns a move of a `test.program.summarize.input`
//! head into a `CallProgram` for the summarize program.

use aether_bloomery_kinds::{CallProgram, HeadMoved, ProgramName};
use aether_bloomery_reactor::reactor;
use aether_test_fixtures_kinds::{SUMMARIZE_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};

pub struct SummarizeCaller;

#[reactor]
impl Reactor for SummarizeCaller {
    const NAMESPACE: &'static str = "test.bloomery.summarize.caller";

    #[rule]
    fn call_summarize(&self, change: HeadMoved<SummarizeInput>) -> CallProgram {
        CallProgram {
            program: SUMMARIZE_BUNDLE,
            name: ProgramName::new(SUMMARIZE_PROGRAM).expect("SUMMARIZE_PROGRAM is const-asserted valid"),
            input: change.to().digest(),
        }
    }
}

aether_actor::export!(SummarizeCaller, generators = [aether_bloomery_bundle::bundle]);
