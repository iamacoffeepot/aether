//! End-to-end: the driver follows the reactor set over a live journal, activating bundles and recording reactions.

use std::error::Error;
use std::fs;

use aether_bloomery_journal::{Batch, JournalReader, Seq};
use aether_bloomery_kinds::{
    Activated, ActivationRejected, Call, CallOutcome, Digest, EncodedArtifact, Head, MoveHead, MoveHeadResult,
    NativeOrigin, OpaqueBytes, Processed, ProgramName, ProgramRef, ReactionFailed, ReactorName, ReactorSet,
    RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, RuleName, Transition, Utf8Text,
};
use aether_bloomery_view::{Activations, HeadActivation, Heads};
use aether_harness_bloomery::{BloomeryHarness, Record};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_test_fixtures_kinds::{
    ASYNC_SUMMARIZE_PROGRAM, MIXED_BUNDLE, RESOLVER_INPUT, RESOLVER_PUBLISHED, ResolverReceipt, ResolverValue,
    SUMMARIZE_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput,
};

/// The input head the reactor fixtures watch.
const INPUT: Head<SummarizeInput> = Head::new("test.bloomery.summarize.input");
const TEXT: Head<Utf8Text> = Head::new("test.bloomery.summarize.text");

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.result")]
struct SummarizeResult {
    text: Ref<Utf8Text>,
}

/// Which fixture wasms to seed: split program/reactor bundles, or one mixed bundle.
#[derive(Clone, Copy)]
enum Bundles<'a> {
    Split { program: &'a [u8], reactor: &'a [u8] },
    Mixed(&'a [u8]),
}

/// Handles the seed staged: the program and reactor bundle digests (one digest for a mixed bundle),
/// the unmoved set, and the call input.
struct Seed {
    program: Digest,
    reactor: Digest,
    set: Digest,
    input: Digest,
    text: Ref<Utf8Text>,
    member: Head<OpaqueBytes>,
}

/// Stage the fixture wasm(s), the unmoved set, and referenced text. The split
/// fixture must create its input in the reactor; the mixed fixture retains its
/// stored-input scenario and stages the wrapper here.
fn seed_batch(bundles: Bundles<'_>) -> Result<(Batch, Seed), Box<dyn Error>> {
    let mut batch = Batch::new();
    let (program, reactor, member) = match bundles {
        Bundles::Split { program, reactor } => {
            let program = batch.stage_bytes(program);
            batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&SUMMARIZE_BUNDLE), program.digest()), None)?;
            let member: Head<OpaqueBytes> = Head::new("test.bloomery.summarize.caller");
            let reactor = batch.stage_bytes(reactor);
            batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&member), reactor.digest()), None)?;
            (program.digest(), reactor.digest(), member)
        }
        Bundles::Mixed(wasm) => {
            let mixed = batch.stage_bytes(wasm);
            batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&MIXED_BUNDLE), mixed.digest()), None)?;
            let member: Head<OpaqueBytes> = Head::new("test.bloomery.mixed.caller");
            batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&member), mixed.digest()), None)?;
            (mixed.digest(), mixed.digest(), member)
        }
    };
    let set = batch.stage_encoded(&ReactorSet::new(vec![member.clone()])?)?;
    let text = batch.stage_text("hello");
    let input = EncodedArtifact::new(&SummarizeInput { text })?;
    let input_digest = input.digest();
    if matches!(bundles, Bundles::Mixed(_)) {
        assert_eq!(batch.stage_artifact(input), input_digest);
    }
    Ok((batch, Seed { program, reactor, set: set.digest(), input: input_digest, text, member }))
}

/// Run the shared reaction flow: settle, set move, settle, input move on
/// `test.bloomery.summarize.input`, settle. Returns the harness and the `Activated` seq.
fn drive_reaction(batch: Batch, seed: &Seed) -> (BloomeryHarness, Seq) {
    let mut harness = BloomeryHarness::start([batch]);
    assert_eq!(harness.settle(Seq(2)), Seq(2));
    assert_eq!(
        harness.move_head(&MoveHead::new(&ReactorSet::ROOT, Ref::from_digest(seed.set), 2)),
        MoveHeadResult::Committed { seq: 3 }
    );
    let activated = harness.settle(Seq(3));
    assert_eq!(
        harness.move_head(&MoveHead::new(&INPUT, Ref::from_digest(seed.input), activated.0)),
        MoveHeadResult::Committed { seq: activated.0 + 1 }
    );
    harness.settle(Seq(activated.0 + 1));
    (harness, activated)
}

/// Run the supplied-input reaction flow. The wrapper digest is computed for
/// expectations but remains absent until the routing append commits it.
fn drive_fresh_reaction(batch: Batch, seed: &Seed) -> (BloomeryHarness, Seq) {
    let mut harness = BloomeryHarness::start([batch]);
    assert_eq!(harness.settle(Seq(2)), Seq(2));
    assert_eq!(
        harness.move_head(&MoveHead::new(&ReactorSet::ROOT, Ref::from_digest(seed.set), 2)),
        MoveHeadResult::Committed { seq: 3 }
    );
    let activated = harness.settle(Seq(3));
    assert!(!harness.stores(&seed.input), "expected input preparation must not stage the wrapper");
    assert_eq!(
        harness.move_head(&MoveHead::new(&TEXT, seed.text, activated.0)),
        MoveHeadResult::Committed { seq: activated.0 + 1 }
    );
    harness.settle(Seq(activated.0 + 1));
    (harness, activated)
}

/// The records one reaction appends after the seed's two head moves: the set
/// move, the activation it causes, the input move, and the reaction's
/// `Requested` → `Transition` pair. Exact, so it also rules out a second
/// request and every failure record (`ReactionFailed`, `ActivationRejected`,
/// `Fault`).
fn reaction_records(seed: &Seed, reactor: &str) -> Result<Vec<Record>, Box<dyn Error>> {
    let program = ProgramRef::new(seed.program, ProgramName::new(SUMMARIZE_PROGRAM)?);
    let recorded_program = program.clone();
    let input = seed.input;
    Ok(vec![
        Record::equal(None, RecordedHeadMove::new(RecordedHead::from(&ReactorSet::ROOT), seed.set)),
        Record::equal(Some(Seq(3)), Activated::new(seed.member.clone(), seed.reactor, Seq(4))?),
        Record::equal(None, RecordedHeadMove::new(RecordedHead::from(&INPUT), seed.input)),
        Record::equal(
            Some(Seq(5)),
            Requested {
                program,
                input: seed.input,
                source: RequestSource::Reaction {
                    bundle: seed.reactor,
                    reactor: ReactorName::new(reactor)?,
                    rule: RuleName::new("call_summarize")?,
                    ordinal: 0,
                },
            },
        ),
        Record::matching(Some(Seq(6)), move |transition: &Transition| {
            assert_eq!(transition.program, recorded_program);
            assert_eq!(transition.input, input);
        }),
    ])
}

fn fresh_reaction_records(seed: &Seed) -> Result<Vec<Record>, Box<dyn Error>> {
    let program = ProgramRef::new(seed.program, ProgramName::new(SUMMARIZE_PROGRAM)?);
    let recorded_program = program.clone();
    let input = seed.input;
    Ok(vec![
        Record::equal(None, RecordedHeadMove::new(RecordedHead::from(&ReactorSet::ROOT), seed.set)),
        Record::equal(Some(Seq(3)), Activated::new(seed.member.clone(), seed.reactor, Seq(4))?),
        Record::equal(None, RecordedHeadMove::new(RecordedHead::from(&TEXT), seed.text.digest())),
        Record::equal(
            Some(Seq(5)),
            Requested {
                program,
                input,
                source: RequestSource::Reaction {
                    bundle: seed.reactor,
                    reactor: ReactorName::new("test.bloomery.summarize.caller")?,
                    rule: RuleName::new("call_selected_text")?,
                    ordinal: 0,
                },
            },
        ),
        Record::matching(Some(Seq(6)), move |transition: &Transition| {
            assert_eq!(transition.program, recorded_program);
            assert_eq!(transition.input, input);
        }),
    ])
}

/// Stage two texts and move one head, returning the staged-but-unmoved text for the live move.
fn seed_barrier_batch() -> Result<(Batch, Digest), Box<dyn Error>> {
    let mut batch = Batch::new();
    let first = batch.stage_text("one");
    let second = batch.stage_text("two");
    let head: Head<Utf8Text> = Head::new("test.bloomery.barrier.first");
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&head), first.digest()), None)?;
    Ok((batch, second.digest()))
}

fn stage_resolver_receipt(
    batch: &mut Batch,
    marker: u64,
    value: u64,
) -> Result<(Ref<ResolverValue>, Ref<ResolverReceipt>), Box<dyn Error>> {
    let value = batch.stage_encoded(&ResolverValue { value })?;
    let receipt = batch.stage_encoded(&ResolverReceipt { value, marker })?;
    Ok((value, receipt))
}

fn activate(mut harness: BloomeryHarness, seed: &Seed, through: Seq) -> (BloomeryHarness, Seq) {
    assert_eq!(harness.settle(through), through);
    assert_eq!(
        harness.move_head(&MoveHead::new(&ReactorSet::ROOT, Ref::from_digest(seed.set), through.0)),
        MoveHeadResult::Committed { seq: through.0 + 1 }
    );
    let activated = harness.settle(Seq(through.0 + 1));
    (harness, activated)
}

#[test]
fn await_processed_waits_for_a_live_append_it_is_woken_for() -> Result<(), Box<dyn Error>> {
    // Catches a warn-dropped `WatchHeadResult` (routing never wakes for another
    // writer's append, so the barrier never answers), an unhandled
    // `AwaitProcessed`, and a barrier answered before its bound is routed.
    let (batch, staged) = seed_barrier_batch()?;
    let mut harness = BloomeryHarness::start([batch]);
    assert_eq!(harness.settle(Seq(1)), Seq(1));

    let barrier = harness.await_processed(Seq(2));
    let head = Head::<Utf8Text>::new("test.bloomery.barrier.second");
    assert_eq!(
        harness.move_head(&MoveHead::new(&head, Ref::from_digest(staged), 1)),
        MoveHeadResult::Committed { seq: 2 }
    );
    assert_eq!(harness.wait(barrier), Processed::Head { head: 2 });
    Ok(())
}

#[test]
fn reactor_set_move_activates_and_persists_a_fresh_input_before_invocation() -> Result<(), Box<dyn Error>> {
    // Catches a warn-dropped `Warmed` / `Evaluated` (routing stalls and `settle`
    // times out), a reactor loaded under the program export, a reply routed to
    // the wrong continuation, a reaction `Requested` that is uncaused, carries a
    // `Native` source, or resolves its bundle at the wrong boundary, a
    // `Transition` not caused by the reaction's `Requested`, and a barrier that
    // answers between `Requested` and `Transition`.
    let Some(reactor_path) = require_wasm("aether_test_fixtures_reactor_call") else {
        return Ok(());
    };
    let Some(program_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let reactor_wasm = fs::read(&reactor_path)?;
    let program_wasm = fs::read(&program_path)?;
    let (batch, seed) = seed_batch(Bundles::Split { program: &program_wasm, reactor: &reactor_wasm })?;

    let (harness, activated) = drive_fresh_reaction(batch, &seed);
    assert_eq!(activated, Seq(4));
    harness.assert_appended(Seq(2), &fresh_reaction_records(&seed)?);
    assert_eq!(harness.head(), Seq(7));
    let transition = harness.record::<Transition>(Seq(7));
    assert!(harness.stores(&seed.input), "the routing append stores the input wrapper");
    assert!(harness.stores(&transition.result), "the staged result is stored");
    assert_eq!(transition.input, seed.input);
    let reader = JournalReader::open(harness.journal_path())?;
    assert_eq!(
        reader.get::<SummarizeInput>(&seed.input)?,
        Some(SummarizeInput { text: seed.text }),
        "the stored wrapper retains the reactor-selected text reference"
    );
    let result = reader.get::<SummarizeResult>(&transition.result)?.expect("stored summarize result");
    assert_eq!(result.text, Ref::of_text("summary:hello"), "real Wasm read the cited input and produced its result");
    assert!(harness.stores(&result.text.digest()), "the result's cited summary text is stored");
    assert_eq!(
        harness.fold::<Activations>().get(&seed.member),
        Some(&HeadActivation::Live(Activated::new(seed.member.clone(), seed.reactor, Seq(4))?)),
        "the member head is served by the activation the set move caused"
    );
    Ok(())
}

#[test]
fn a_mixed_bundle_serves_its_reactor_and_its_program_from_one_load() -> Result<(), Box<dyn Error>> {
    // Catches a second `LoadComponent` for a digest one role already loaded: the component host refuses the unit's bundle name with `SubnameInUse`, which the driver records as a `BundleUnavailable` `Fault`, so one load is the only way both roles succeed.
    let Some(mixed_path) = require_wasm("aether_test_fixtures_mixed_bundle") else {
        return Ok(());
    };
    let mixed_wasm = fs::read(&mixed_path)?;
    let (batch, seed) = seed_batch(Bundles::Mixed(&mixed_wasm))?;
    assert_eq!(seed.program, seed.reactor);

    let (harness, activated) = drive_reaction(batch, &seed);
    assert_eq!(activated, Seq(4));
    harness.assert_appended(Seq(2), &reaction_records(&seed, "test.bloomery.mixed.caller")?);
    let transition = harness.record::<Transition>(Seq(7));
    assert!(harness.stores(&transition.result), "the staged result is stored");
    Ok(())
}

#[test]
fn historical_and_live_resolving_views_produce_the_same_intent() -> Result<(), Box<dyn Error>> {
    let Some(reactor_path) = require_wasm("aether_test_fixtures_reactor") else {
        return Ok(());
    };
    let Some(program_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let reactor_wasm = fs::read(&reactor_path)?;
    let program_wasm = fs::read(&program_path)?;
    let (mut batch, seed) = seed_batch(Bundles::Split { program: &program_wasm, reactor: &reactor_wasm })?;
    let (_, historical) = stage_resolver_receipt(&mut batch, 1, 42)?;
    let (_, live) = stage_resolver_receipt(&mut batch, 2, 42)?;
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&RESOLVER_INPUT), historical.digest()), None)?;

    let (mut harness, activated) = activate(BloomeryHarness::start([batch]), &seed, Seq(3));
    assert_eq!(activated, Seq(5), "activation waits for historical artifact-backed warming");
    assert_eq!(
        harness.move_head(&MoveHead::new(&RESOLVER_INPUT, live, activated.0)),
        MoveHeadResult::Committed { seq: 6 }
    );
    assert_eq!(harness.settle(Seq(6)), Seq(7));
    assert_eq!(harness.fold::<Heads>().get(&RESOLVER_PUBLISHED), Some(live));
    Ok(())
}

#[test]
fn mixed_bundle_correlates_simultaneous_program_and_view_fetches() -> Result<(), Box<dyn Error>> {
    let Some(mixed_path) = require_wasm("aether_test_fixtures_mixed_bundle") else {
        return Ok(());
    };
    let mixed_wasm = fs::read(&mixed_path)?;
    let (mut batch, seed) = seed_batch(Bundles::Mixed(&mixed_wasm))?;
    let (_, receipt) = stage_resolver_receipt(&mut batch, 3, 42)?;
    let (mut harness, activated) = activate(BloomeryHarness::start([batch]), &seed, Seq(2));
    assert_eq!(activated, Seq(4));

    let moved = harness.begin_move_head(&MoveHead::new(&RESOLVER_INPUT, receipt, activated.0));
    let call = harness.begin_call(&Call {
        program: MIXED_BUNDLE,
        name: ProgramName::new(ASYNC_SUMMARIZE_PROGRAM)?,
        input: seed.input,
        origin: NativeOrigin::new("test.resolver.concurrent")?,
        key: 91,
    });
    assert_eq!(harness.wait(moved), MoveHeadResult::Committed { seq: 5 });
    assert!(matches!(harness.wait(call), CallOutcome::Transition { key: 91, .. }));
    let _ = harness.settle(Seq(5));
    assert_eq!(harness.fold::<Heads>().get(&RESOLVER_PUBLISHED), Some(receipt));
    Ok(())
}

#[test]
fn missing_nested_artifact_fails_the_real_wasm_fold_without_an_intent() -> Result<(), Box<dyn Error>> {
    let Some(reactor_path) = require_wasm("aether_test_fixtures_reactor") else {
        return Ok(());
    };
    let Some(program_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let reactor_wasm = fs::read(&reactor_path)?;
    let program_wasm = fs::read(&program_path)?;
    let (mut batch, seed) = seed_batch(Bundles::Split { program: &program_wasm, reactor: &reactor_wasm })?;
    let (value, receipt) = stage_resolver_receipt(&mut batch, 4, 42)?;
    let (mut harness, activated) = activate(BloomeryHarness::start([batch]), &seed, Seq(2));
    assert_eq!(activated, Seq(4));

    let hex = value.digest().to_string();
    fs::remove_file(harness.journal_path().join("blobs").join(&hex[..2]).join(hex))?;
    assert_eq!(
        harness.move_head(&MoveHead::new(&RESOLVER_INPUT, receipt, activated.0)),
        MoveHeadResult::Committed { seq: 5 }
    );
    assert_eq!(harness.settle(Seq(5)), Seq(7));
    let failed = harness.record::<ReactionFailed>(Seq(6));
    assert_eq!(failed.bundle, seed.reactor);
    assert_eq!(failed.reactor, None);
    assert!(failed.reason.as_str().contains("could not be read"), "{}", failed.reason.as_str());
    let rejected = harness.record::<ActivationRejected>(Seq(7));
    assert_eq!(rejected.head, seed.member);
    assert_eq!(rejected.bundle, seed.reactor);
    assert_eq!(rejected.reason, failed.reason);
    assert_eq!(harness.fold::<Heads>().get(&RESOLVER_PUBLISHED), None);
    Ok(())
}
