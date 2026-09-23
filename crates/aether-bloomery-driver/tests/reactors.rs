//! End-to-end: the driver follows the reactor set over a live journal, activating bundles and recording reactions.

use std::error::Error;
use std::fs;

use aether_bloomery_journal::{Batch, Seq};
use aether_bloomery_kinds::{
    Activated, Digest, Head, MoveHead, MoveHeadResult, OpaqueBytes, ProgramName, ProgramRef, ReactorName, ReactorSet,
    RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, RuleName, Transition, Utf8Text,
};
use aether_bloomery_view::{Activations, HeadActivation};
use aether_harness_bloomery::{BloomeryHarness, Record};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_test_fixtures_kinds::{MIXED_BUNDLE, SUMMARIZE_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};

/// The input head the reactor fixtures watch.
const INPUT: Head<SummarizeInput> = Head::new("test.bloomery.summarize.input");

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
    member: Head<OpaqueBytes>,
}

/// Stage the fixture wasm(s) under their heads, plus the unmoved set and one call input.
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
    let input = batch.stage_encoded(&SummarizeInput { text })?;
    Ok((batch, Seed { program, reactor, set: set.digest(), input: input.digest(), member }))
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

/// Stage two texts and move one head, returning the staged-but-unmoved text for the live move.
fn seed_barrier_batch() -> Result<(Batch, Digest), Box<dyn Error>> {
    let mut batch = Batch::new();
    let first = batch.stage_text("one");
    let second = batch.stage_text("two");
    let head: Head<Utf8Text> = Head::new("test.bloomery.barrier.first");
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&head), first.digest()), None)?;
    Ok((batch, second.digest()))
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
    assert_eq!(harness.wait(barrier).head, 2);
    Ok(())
}

#[test]
fn reactor_set_move_activates_and_its_call_program_records_requested_then_transition() -> Result<(), Box<dyn Error>> {
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

    let (harness, activated) = drive_reaction(batch, &seed);
    assert_eq!(activated, Seq(4));
    harness.assert_appended(Seq(2), &reaction_records(&seed, "test.bloomery.summarize.caller")?);
    assert_eq!(harness.head(), Seq(7));
    let transition = harness.record::<Transition>(Seq(7));
    assert!(harness.stores(&transition.result), "the staged result is stored");
    assert_eq!(
        harness.fold::<Activations>().get(&seed.member),
        Some(&HeadActivation::Live(Activated::new(seed.member.clone(), seed.reactor, Seq(4))?)),
        "the member head is served by the activation the set move caused"
    );
    Ok(())
}

#[test]
fn a_mixed_bundle_serves_its_reactor_and_its_program_from_one_load() -> Result<(), Box<dyn Error>> {
    // Catches a second `LoadComponent` for a digest one role already loaded: the component host refuses the digest name with `SubnameInUse`, which the driver records as a `BundleUnavailable` `Fault`, so one load is the only way both roles succeed.
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
