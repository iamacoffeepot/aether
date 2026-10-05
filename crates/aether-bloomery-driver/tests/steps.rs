//! End-to-end: every ticketed step the driver performs for a program call is a span under the step target, and each
//! one closes when its reply arrives.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fs;
use std::mem;
use std::sync::{Arc, Mutex, PoisonError};

use aether_bloomery_journal::Batch;
use aether_bloomery_kinds::{Call, CallOutcome, Head, NativeOrigin, ProgramName, RecordedHead, RecordedHeadMove};
use aether_data::{OpaqueBytes, Ref, Utf8Text};
use aether_harness_bloomery::BloomeryHarness;
use aether_harness_substrate::test_helpers::require_wasm;
use tracing::span::{Attributes, Id};
use tracing::{Subscriber, subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// The target a subscriber filters the driver's step spans on.
const STEP_TARGET: &str = "aether.bloomery.step";

/// Local mirror of the fixture's `test.program.summarize.input`: same kind
/// name, same shape, so it encodes to the same digest the guest expects.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

/// The `program` head the seed binds to the fixture bundle.
const PROGRAM: Head<OpaqueBytes> = Head::new("program");

/// The step spans a [`StepCounter`] saw: each open one by id, and how many of
/// each name closed.
#[derive(Debug, Default)]
struct Tally {
    open: HashMap<Id, &'static str>,
    closed: BTreeMap<&'static str, usize>,
}

/// A layer that tallies the spans opened and closed under [`STEP_TARGET`].
#[derive(Clone, Default)]
struct StepCounter(Arc<Mutex<Tally>>);

impl<S: Subscriber> Layer<S> for StepCounter {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let metadata = attrs.metadata();
        if metadata.target() == STEP_TARGET {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).open.insert(id.clone(), metadata.name());
        }
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        let mut tally = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(name) = tally.open.remove(&id) {
            *tally.closed.entry(name).or_default() += 1;
        }
    }
}

#[test]
fn every_step_span_of_a_program_call_closes_at_its_reply() -> Result<(), Box<dyn Error>> {
    // Catches a reply path that forgets to close its step's span (one leaked span per request), a step that is never
    // spanned, and a span opened under another target, which a subscriber filtering on the step target never sees.
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let steps = StepCounter::default();
    subscriber::set_global_default(Registry::default().with(steps.clone()))?;

    let wasm = fs::read(&wasm_path)?;
    let mut seed = Batch::new();
    let bundle = seed.stage_bytes(&wasm);
    seed.push_event(&RecordedHeadMove::new(RecordedHead::from(&PROGRAM), bundle.digest()), None)?;
    let text = seed.stage_text("hello");
    let input = seed.stage_encoded(&SummarizeInput { text })?.digest();

    let mut harness = BloomeryHarness::start([seed]);
    let summarize = Call {
        program: PROGRAM,
        name: ProgramName::new("test.program.summarize")?,
        input,
        origin: NativeOrigin::new("test.driver")?,
        key: 1,
    };
    let outcome = harness.call(&summarize);
    assert!(matches!(outcome, CallOutcome::Transition { key: 1, .. }), "the call runs the program: {outcome:?}");
    let head = harness.head();
    harness.settle(head);

    let Tally { open, closed } = mem::take(&mut *steps.0.lock().unwrap_or_else(PoisonError::into_inner));
    assert!(open.is_empty(), "no step span outlives its step: {:?} still open", open.values());
    for step in ["read_artifact", "read_closure", "load", "invoke", "append"] {
        assert!(closed.get(step).is_some_and(|&count| count > 0), "no {step} span closed: {closed:?}");
    }
    Ok(())
}
