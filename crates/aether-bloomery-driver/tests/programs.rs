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

/// Local mirror of the fixture's `test.program.read_uncited.input`: a bare
/// digest, which the driver's closure read does not follow.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.read_uncited.input")]
struct ReadUncitedInput {
    text: Digest,
}

/// Local mirror of the fixture's `test.program.read_uncited.result`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.read_uncited.result")]
struct ReadUncitedResult {
    text: Ref<Utf8Text>,
}

/// Local mirror of `aether-test-fixtures-program-process`'s `test.program.exec.input`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.exec.input")]
struct ExecInput {
    binary: Ref<Utf8Text>,
}

/// The `program` head the seed binds to the fixture bundle.
const PROGRAM: Head<OpaqueBytes> = Head::new("program");

/// The `process` head the seed binds to the `Process` fixture bundle.
const PROCESS: Head<OpaqueBytes> = Head::new("process");

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

#[test]
fn fetch_on_miss_reads_through_the_mounted_journal() -> Result<(), Box<dyn Error>> {
    // Catches a fetch sent to a position nobody serves (#6478), a relay that loses the invocation's wait, a driver answer that does not reach the bundle root with its correlation (from a journal read or from the cache), and a relay that forwards only `Found`.
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let wasm = fs::read(&wasm_path)?;
    let mut seed = Batch::new();
    let bundle = seed.stage_bytes(&wasm);
    seed.push_event(&RecordedHeadMove::new(RecordedHead::from(&PROGRAM), bundle.digest()), None)?;
    let text = seed.stage_text("hello");
    let found_input = seed.stage_encoded(&ReadUncitedInput { text: text.digest() })?.digest();
    let missing_input = seed.stage_encoded(&ReadUncitedInput { text: Digest::from_bytes([9; 32]) })?.digest();

    let mut harness = BloomeryHarness::start([seed]);
    let origin = NativeOrigin::new("test.driver")?;
    let name = ProgramName::new("test.program.read_uncited")?;
    let found = Call { program: PROGRAM, name: name.clone(), input: found_input, origin: origin.clone(), key: 1 };
    let outcome = harness.call(&found);
    let CallOutcome::Transition { key: 1, transition, .. } = outcome else {
        panic!("expected a Transition outcome, got {outcome:?}");
    };
    let expected = Ref::of_encoded(&ReadUncitedResult { text: Ref::of_text("fetched:hello") })?.digest();
    assert_eq!(transition.result, expected, "the program read the fetched text");
    assert!(harness.stores(&transition.result), "the staged result is stored");

    let missing = Call { program: PROGRAM, name: name.clone(), input: missing_input, origin: origin.clone(), key: 2 };
    match harness.call(&missing) {
        CallOutcome::Fault { key: 2, fault, .. } => {
            assert_eq!(fault.reason, FaultReason::InputMissing, "a missing fetch faults InputMissing");
        }
        other => panic!("expected an InputMissing fault, got {other:?}"),
    }

    let cached = Call { program: PROGRAM, name, input: found_input, origin, key: 3 };
    match harness.call(&cached) {
        CallOutcome::Transition { key: 3, transition, .. } => {
            assert_eq!(transition.result, expected, "the cached fetch answers the program");
        }
        other => panic!("expected a Transition outcome, got {other:?}"),
    }
    Ok(())
}

#[test]
fn a_bundle_with_a_process_program_is_refused_where_process_is_not_composed() -> Result<(), Box<dyn Error>> {
    // Catches an invocation whose `depends` is missing or not load-checked (the load succeeds and the call hangs, #6602), a generator that declares the wrong target (the refusal names another namespace), and a refused load the driver does not record.
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program_process") else {
        return Ok(());
    };
    let wasm = fs::read(&wasm_path)?;
    let mut seed = Batch::new();
    let bundle = seed.stage_bytes(&wasm);
    seed.push_event(&RecordedHeadMove::new(RecordedHead::from(&PROCESS), bundle.digest()), None)?;
    let binary = seed.stage_text("true");
    let input = seed.stage_encoded(&ExecInput { binary })?.digest();

    let mut harness = BloomeryHarness::start([seed]);
    let origin = NativeOrigin::new("test.driver")?;
    let exec = Call { program: PROCESS, name: ProgramName::new("test.program.exec")?, input, origin, key: 1 };
    match harness.call(&exec) {
        CallOutcome::Fault { key: 1, fault, .. } => {
            let FaultReason::BundleUnavailable { reason } = &fault.reason else {
                panic!("a refused load faults BundleUnavailable, got {:?}", fault.reason);
            };
            assert!(
                reason.as_str().contains("depends on aether.process, which is not live"),
                "the refusal names the uncomposed target, got {reason:?}"
            );
            assert_eq!(fault.program.bundle(), bundle.digest());
        }
        other => panic!("expected a BundleUnavailable fault, got {other:?}"),
    }
    harness.assert_appended(Seq(1), &[Record::of::<Requested>(None), Record::of::<Fault>(Some(Seq(2)))]);
    Ok(())
}
