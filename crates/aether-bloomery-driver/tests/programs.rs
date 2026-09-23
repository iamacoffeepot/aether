//! End-to-end: the driver loads the program fixture bundle by digest and records caused outcomes.

use std::error::Error;
use std::fs;

use aether_bloomery_journal::{Batch, Seq};
use aether_bloomery_kinds::{
    Call, CallOutcome, CallRefusal, Digest, Fault, FaultReason, Head, NativeOrigin, OpaqueBytes, ProgramName,
    ProgramRef, RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, Transition, Utf8Text, artifact_digest,
};
use aether_bloomery_view::Heads;
use aether_data::Kind;
use aether_harness_bloomery::{BloomeryHarness, Record};
use aether_harness_substrate::test_helpers::require_wasm;

/// Local mirror of the fixture's `test.program.summarize.input`: same kind
/// name, same shape, so it encodes to the same digest the guest expects.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

/// Local mirror of the fixture's `test.program.refuse.input`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.refuse.input")]
struct RefuseInput {
    marker: u32,
}

/// The `program` head the seed binds to the fixture bundle.
const PROGRAM: Head<OpaqueBytes> = Head::new("program");

#[test]
fn program_calls_record_caused_outcomes_from_one_loaded_root() -> Result<(), Box<dyn Error>> {
    // Catches a broken cause link, an unpinned digest, unstored staged artifacts, a reload per call, re-running a repeated key, and a Call whose chain stays open after its outcome (the journal watch re-armed on it).
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let wasm = fs::read(&wasm_path)?;
    let mut seed = Batch::new();
    let bundle = seed.stage_bytes(&wasm);
    seed.push_event(&RecordedHeadMove::new(RecordedHead::from(&PROGRAM), bundle.digest()), None)?;
    let text = seed.stage_text("hello");
    let summarize_input = seed.stage_encoded(&SummarizeInput { text })?.digest();
    let refuse_input = seed.stage_encoded(&RefuseInput { marker: 1 })?.digest();
    assert_eq!(bundle.digest(), artifact_digest(OpaqueBytes::ID, &wasm));

    let mut harness = BloomeryHarness::start([seed]);
    let origin = NativeOrigin::new("test.driver")?;
    let summarize_program = ProgramRef::new(bundle.digest(), ProgramName::new("test.program.summarize")?);
    let summarize = Call {
        program: PROGRAM,
        name: ProgramName::new("test.program.summarize")?,
        input: summarize_input,
        origin: origin.clone(),
        key: 1,
    };
    let outcome = harness.call(&summarize);
    let CallOutcome::Transition { key: 1, seq, transition } = outcome else {
        panic!("expected a Transition outcome, got {outcome:?}");
    };
    assert_eq!(seq, 3);
    harness.assert_appended(
        Seq(1),
        &[
            Record::equal(
                None,
                Requested {
                    program: summarize_program.clone(),
                    input: summarize_input,
                    source: RequestSource::Native { origin: origin.clone(), key: 1 },
                },
            ),
            Record::matching(Some(Seq(2)), move |recorded: &Transition| {
                assert_eq!(recorded.program, summarize_program);
                assert_eq!(recorded.input, summarize_input);
            }),
        ],
    );
    assert_eq!(transition, harness.record::<Transition>(Seq(3)), "the reply is the record");
    assert!(harness.stores(&transition.result), "the staged result is stored");
    assert_eq!(harness.fold::<Heads>().get(&PROGRAM), Some(bundle), "the seed's binding still serves the call");

    let refuse = Call {
        program: PROGRAM,
        name: ProgramName::new("test.program.refuse")?,
        input: refuse_input,
        origin: origin.clone(),
        key: 2,
    };
    match harness.call(&refuse) {
        CallOutcome::Fault { key: 2, fault, .. } => {
            assert!(
                matches!(fault.reason, FaultReason::Refused { .. }),
                "a refusing program faults Refused, got {:?}",
                fault.reason
            );
            assert_eq!(fault.program.bundle(), bundle.digest());
        }
        other => panic!("expected a Refused fault, got {other:?}"),
    }
    harness.assert_appended(
        Seq(3),
        &[
            Record::matching(None, move |requested: &Requested| {
                assert_eq!(requested.source, RequestSource::Native { origin: origin.clone(), key: 2 });
                assert_eq!(requested.input, refuse_input);
            }),
            Record::of::<Fault>(Some(Seq(4))),
        ],
    );

    let before = harness.head();
    match harness.call(&summarize) {
        CallOutcome::Transition { key: 1, seq: repeated, .. } => assert_eq!(repeated, seq),
        other => panic!("expected the recorded transition, got {other:?}"),
    }
    assert_eq!(harness.head(), before);
    Ok(())
}

#[test]
fn unbound_head_is_refused_and_records_nothing() -> Result<(), Box<dyn Error>> {
    // Catches recording a request that can't resolve.
    let mut harness = BloomeryHarness::start([]);
    let before = harness.head();
    let call = Call {
        program: Head::<OpaqueBytes>::new("missing"),
        name: ProgramName::new("test.program.summarize")?,
        input: Digest::from_bytes([7; 32]),
        origin: NativeOrigin::new("test.driver")?,
        key: 1,
    };
    match harness.call(&call) {
        CallOutcome::Refused { key: 1, reason: CallRefusal::HeadUnbound } => {}
        other => panic!("expected a HeadUnbound refusal, got {other:?}"),
    }
    assert_eq!(harness.head(), before);
    Ok(())
}
