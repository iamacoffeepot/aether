//! Bundle whose one rule turns a move of a `test.program.summarize.input`
//! head into a `CallProgram` for the summarize program.

use aether_bloomery_kinds::{CallInput, CallProgram, Head, HeadMoved, ProgramName, Ref, Utf8Text};
use aether_bloomery_reactor::{Guard, NoViews, reactor};
use aether_bloomery_view::{ViewCursor, view};
use aether_test_fixtures_kinds::{SUMMARIZE_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};

struct SummarizeName(ProgramName);

impl Guard<HeadMoved<SummarizeInput>> for SummarizeName {
    type Views = NoViews;

    fn resolve(_trigger: &HeadMoved<SummarizeInput>, (): ()) -> Option<Self> {
        ProgramName::new(SUMMARIZE_PROGRAM).ok().map(Self)
    }
}

pub struct SummarizeCaller;

const TEXT: Head<Utf8Text> = Head::new("test.bloomery.summarize.text");

#[derive(Default)]
struct SelectedText {
    cursor: ViewCursor,
    selected: Option<Ref<Utf8Text>>,
}

#[view(cursor = cursor)]
impl View for SelectedText {
    #[fold]
    fn moved(&mut self, change: HeadMoved<Utf8Text>) {
        if change.head() == &TEXT {
            self.selected = Some(change.to());
        }
    }
}

struct FreshSummarizeCall(CallProgram);

impl Guard<HeadMoved<Utf8Text>> for FreshSummarizeCall {
    type Views = SelectedText;

    fn resolve(change: &HeadMoved<Utf8Text>, selected: &SelectedText) -> Option<Self> {
        if change.head() != &TEXT {
            return None;
        }
        let text = selected.selected?;
        let name = ProgramName::new(SUMMARIZE_PROGRAM).ok()?;
        CallProgram::with_input(SUMMARIZE_BUNDLE, name, &SummarizeInput { text }).ok().map(Self)
    }
}

#[reactor]
impl Reactor for SummarizeCaller {
    const NAMESPACE: &'static str = "test.bloomery.summarize.caller";

    #[rule]
    fn call_summarize(&self, change: HeadMoved<SummarizeInput>, name: SummarizeName) -> CallProgram {
        CallProgram { program: SUMMARIZE_BUNDLE, name: name.0, input: CallInput::Stored(change.to().digest()) }
    }

    #[rule]
    fn call_selected_text(&self, _change: HeadMoved<Utf8Text>, call: FreshSummarizeCall) -> CallProgram {
        call.0
    }
}

aether_actor::export!(public = [SummarizeCaller], generators = [aether_bloomery_bundle::bundle]);
