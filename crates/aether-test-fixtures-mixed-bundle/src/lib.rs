//! Mixed program-and-reactor WASM bundle: two programs plus one reactor whose
//! rules call a program and resolve immutable artifacts.

use aether_actor::export;
use aether_bloomery_kinds::{CallInput, CallProgram, HeadMoved, Mode, ProgramName, Ref, Refusal, SetHeads, Utf8Text};
use aether_bloomery_program::{Async, Env, Program, Sync, program};
use aether_bloomery_reactor::{Guard, NoViews, reactor};
use aether_bloomery_view::{ArtifactResolver, ResolveError, ViewCursor, view};
use aether_test_fixtures_kinds::{
    MIXED_BUNDLE, RESOLVER_INPUT, RESOLVER_PUBLISHED, ResolverReceipt, ResolverValue, SUMMARIZE_PROGRAM, SummarizeInput,
};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.mixed.summary")]
struct MixedSummary {
    text: Ref<Utf8Text>,
}

struct Summarize;

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

struct AsyncSummarize;

#[program]
impl Program for AsyncSummarize {
    const NAME: &'static str = "test.program.async_summarize";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read cited text asynchronously beside a resolving view.";
    type Input = SummarizeInput;
    type Result = MixedSummary;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let text = env.read_text(input.text).await?;
        Ok(MixedSummary { text: env.stage_text(&format!("async-summary:{text}")) })
    }
}

struct SummarizeName(ProgramName);

impl Guard<HeadMoved<SummarizeInput>> for SummarizeName {
    type Views = NoViews;

    fn resolve(_trigger: &HeadMoved<SummarizeInput>, (): ()) -> Option<Self> {
        ProgramName::new(SUMMARIZE_PROGRAM).ok().map(Self)
    }
}

pub struct MixedCaller;

#[derive(Default)]
struct ResolvedReceipts {
    cursor: ViewCursor,
    receipt: Option<Ref<ResolverReceipt>>,
    value: u64,
}

#[view(cursor = cursor)]
impl aether_bloomery_view::View for ResolvedReceipts {
    #[fold]
    async fn moved(
        &mut self,
        change: HeadMoved<ResolverReceipt>,
        artifacts: &mut ArtifactResolver,
    ) -> Result<(), ResolveError> {
        if change.head() != &RESOLVER_INPUT {
            return Ok(());
        }
        let receipt = artifacts.read(change.to()).await?;
        let ResolverValue { value } = artifacts.read(receipt.value).await?;
        self.receipt = Some(change.to());
        self.value = value;
        Ok(())
    }
}

struct Resolved(Ref<ResolverReceipt>);

impl Guard<HeadMoved<ResolverReceipt>> for Resolved {
    type Views = ResolvedReceipts;

    fn resolve(trigger: &HeadMoved<ResolverReceipt>, view: &ResolvedReceipts) -> Option<Self> {
        (trigger.head() == &RESOLVER_INPUT && view.receipt == Some(trigger.to()) && view.value == 42)
            .then_some(Self(trigger.to()))
    }
}

#[reactor]
impl Reactor for MixedCaller {
    const NAMESPACE: &'static str = "test.bloomery.mixed.caller";

    #[rule]
    fn call_summarize(&self, change: HeadMoved<SummarizeInput>, name: SummarizeName) -> CallProgram {
        CallProgram { program: MIXED_BUNDLE, name: name.0, input: CallInput::Stored(change.to().digest()) }
    }

    #[rule]
    fn publish_resolved(&self, _change: HeadMoved<ResolverReceipt>, resolved: Resolved) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&RESOLVER_PUBLISHED, None, resolved.0)])
    }
}

export!(public = [Summarize, AsyncSummarize, MixedCaller], generators = [aether_bloomery_bundle::bundle]);

const _: Summarize = Summarize;
const _: AsyncSummarize = AsyncSummarize;
