//! Fold semantics of [`Requests`]: outstanding program requests, their dedup key, and the reaction watermark.

use std::error::Error;

use aether_bloomery_kinds::{
    Activated, Detail, Digest, Entry, Fault, FaultReason, Head, NativeOrigin, OpaqueBytes, ProgramName, ProgramRef,
    ReactionFailed, ReactorName, RecordedHead, RecordedHeadMove, RequestSource, Requested, RuleName, Seq, Transition,
};
use aether_bloomery_view::{Outcome, Request, RequestFoldError, Requests, SequenceError};
use aether_data::{Storage, StorageData};

fn entry_for<K: Storage + Clone>(seq: u64, cause: Option<u64>, event: &K) -> Result<Entry, Box<dyn Error>> {
    Ok(Entry {
        seq: Seq(seq),
        kind: K::ID,
        cause: cause.map(Seq),
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(event.clone()))?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.requests.note")]
struct Note {
    n: u64,
}

fn digest(byte: u8) -> Digest {
    Digest::from_bytes([byte; 32])
}

fn program(name: &str) -> Result<ProgramRef, Box<dyn Error>> {
    Ok(ProgramRef::new(digest(1), ProgramName::new(name)?))
}

fn native(origin: &str, key: u64) -> Result<RequestSource, Box<dyn Error>> {
    Ok(RequestSource::Native { origin: NativeOrigin::new(origin)?, key })
}

fn reaction(reactor: &str, rule: &str, ordinal: u32) -> Result<RequestSource, Box<dyn Error>> {
    Ok(RequestSource::Reaction {
        bundle: digest(9),
        reactor: ReactorName::new(reactor)?,
        rule: RuleName::new(rule)?,
        ordinal,
    })
}

#[test]
fn a_second_outcome_for_one_request_is_refused() -> Result<(), Box<dyn Error>> {
    // Bug: last-write-wins outcome, which would make a replayed `Call` answer differently and let restart fault a finished request.
    let program_ref = program("trim")?;
    let input = digest(2);
    let source = native("cli", 1)?;
    let mut requests = Requests::new();
    requests.apply(&entry_for(1, None, &Requested { program: program_ref.clone(), input, source })?)?;
    requests.apply(&entry_for(2, Some(1), &Transition { program: program_ref.clone(), input, result: digest(3) })?)?;

    let second = requests
        .apply(&entry_for(3, Some(1), &Fault { program: program_ref, input, reason: FaultReason::Interrupted })?)
        .expect_err("second outcome must refuse");
    match second {
        RequestFoldError::SecondOutcome { seq, request, first } => {
            assert_eq!(seq, Seq(3));
            assert_eq!(request, Seq(1));
            assert_eq!(first, Seq(2));
        }
        other => panic!("expected SecondOutcome, got {other:?}"),
    }

    assert_eq!(requests.cursor(), Seq(2));
    let outcome = requests.get(Seq(1)).expect("request 1 recorded").outcome().expect("outcome recorded");
    assert!(matches!(outcome, Outcome::Transition { seq, .. } if *seq == Seq(2)));
    Ok(())
}

#[test]
fn a_fault_completes_its_request() -> Result<(), Box<dyn Error>> {
    // Bug: a fold that leaves a faulted request outstanding, so restart would record a second `Interrupted` for it.
    let program_one = program("one")?;
    let program_two = program("two")?;
    let input = digest(2);
    let source_one = native("cli", 1)?;
    let source_two = native("cli", 2)?;
    let mut requests = Requests::new();
    requests.apply(&entry_for(
        1,
        None,
        &Requested { program: program_one.clone(), input, source: source_one.clone() },
    )?)?;
    requests.apply(&entry_for(2, None, &Requested { program: program_two, input, source: source_two })?)?;
    requests.apply(&entry_for(
        3,
        Some(1),
        &Fault { program: program_one, input, reason: FaultReason::Interrupted },
    )?)?;

    let outstanding: Vec<Seq> = requests.outstanding().map(Request::seq).collect();
    assert_eq!(outstanding, vec![Seq(2)]);

    let completed = requests.find(None, &source_one).expect("request 1 recorded by dedup key");
    assert_eq!(completed.seq(), Seq(1));
    assert!(matches!(completed.outcome(), Some(Outcome::Fault { seq, .. }) if *seq == Seq(3)));
    Ok(())
}

#[test]
fn an_outcome_must_answer_its_recorded_request() -> Result<(), Box<dyn Error>> {
    // Bug: outcomes silently dropped or attached to the wrong request.
    let program_ref = program("trim")?;
    let input = digest(2);
    let source = native("cli", 1)?;
    let mut requests = Requests::new();
    requests.apply(&entry_for(1, None, &Requested { program: program_ref.clone(), input, source })?)?;
    requests.apply(&entry_for(2, None, &Note { n: 7 })?)?;

    let unknown = requests
        .apply(&entry_for(3, Some(2), &Transition { program: program_ref.clone(), input, result: digest(3) })?)
        .expect_err("cause naming an unrelated entry must refuse");
    match unknown {
        RequestFoldError::UnknownRequest { seq, cause } => {
            assert_eq!(seq, Seq(3));
            assert_eq!(cause, Seq(2));
        }
        other => panic!("expected UnknownRequest, got {other:?}"),
    }

    let uncaused = requests
        .apply(&entry_for(3, None, &Transition { program: program_ref.clone(), input, result: digest(3) })?)
        .expect_err("uncaused outcome must refuse");
    assert!(matches!(uncaused, RequestFoldError::UncausedOutcome { seq } if seq == Seq(3)));

    let mismatched = requests
        .apply(&entry_for(3, Some(1), &Transition { program: program_ref, input: digest(4), result: digest(3) })?)
        .expect_err("mismatched input must refuse");
    assert!(
        matches!(mismatched, RequestFoldError::OutcomeMismatch { seq, request } if seq == Seq(3) && request == Seq(1))
    );

    assert_eq!(requests.cursor(), Seq(2));
    assert!(requests.get(Seq(1)).expect("request 1 recorded").outcome().is_none());
    let outstanding: Vec<Seq> = requests.outstanding().map(Request::seq).collect();
    assert_eq!(outstanding, vec![Seq(1)]);
    Ok(())
}

#[test]
fn a_repeated_dedup_key_is_refused() -> Result<(), Box<dyn Error>> {
    // Bug: an index that overwrites the first request, or accepts a key the driver could never have written.
    let program_ref = program("trim")?;
    let input = digest(2);
    let source = native("cli", 1)?;
    let mut requests = Requests::new();
    requests.apply(&entry_for(1, None, &Requested { program: program_ref.clone(), input, source: source.clone() })?)?;

    let duplicate = requests
        .apply(&entry_for(2, None, &Requested { program: program_ref.clone(), input, source: source.clone() })?)
        .expect_err("repeated dedup key must refuse");
    match duplicate {
        RequestFoldError::DuplicateRequest { seq, first } => {
            assert_eq!(seq, Seq(2));
            assert_eq!(first, Seq(1));
        }
        other => panic!("expected DuplicateRequest, got {other:?}"),
    }
    assert_eq!(requests.find(None, &source).expect("request 1 still recorded").seq(), Seq(1));

    let mismatched_cause = requests
        .apply(&entry_for(
            2,
            None,
            &Requested { program: program_ref, input, source: reaction("rain", "on_tick", 0)? },
        )?)
        .expect_err("reaction source with no cause must refuse");
    assert!(matches!(mismatched_cause, RequestFoldError::SourceCause { seq } if seq == Seq(2)));

    assert_eq!(requests.cursor(), Seq(1));
    Ok(())
}

#[test]
fn reaction_watermark_is_the_highest_reaction_sourced_cause() -> Result<(), Box<dyn Error>> {
    // Bug: counting activation records (restart would warm past unrecorded reactions), missing caused head moves
    // (restart would re-evaluate a seq whose move is recorded), or last-wins instead of max.
    let source = native("cli", 1)?;
    let program_ref = program("trim")?;
    let head = Head::<OpaqueBytes>::new("main");
    let recorded_head = RecordedHead::from(&head);
    let mut requests = Requests::new();

    requests.apply(&entry_for(1, None, &Requested { program: program_ref.clone(), input: digest(2), source })?)?;
    requests.apply(&entry_for(2, None, &RecordedHeadMove::new(recorded_head.clone(), digest(3)))?)?;
    requests.apply(&entry_for(3, Some(2), &Activated::new(head, digest(4), Seq(1))?)?)?;
    assert_eq!(requests.reaction_watermark(), Seq(0));

    requests.apply(&entry_for(
        4,
        Some(2),
        &Requested { program: program_ref, input: digest(2), source: reaction("rain", "on_tick", 0)? },
    )?)?;
    requests.apply(&entry_for(
        5,
        Some(4),
        &ReactionFailed { bundle: digest(5), reactor: None, reason: Detail::new("boom") },
    )?)?;
    requests.apply(&entry_for(6, Some(5), &RecordedHeadMove::new(recorded_head, digest(6)))?)?;

    assert_eq!(requests.reaction_watermark(), Seq(5));
    Ok(())
}

#[test]
fn a_gap_is_refused() -> Result<(), Box<dyn Error>> {
    // Bug: a fold that skips the shared next-sequence check.
    let program_ref = program("trim")?;
    let source = native("cli", 1)?;
    let mut requests = Requests::new();
    requests.apply(&entry_for(
        1,
        None,
        &Requested { program: program_ref, input: digest(2), source: source.clone() },
    )?)?;

    let gap = requests.apply(&entry_for(3, None, &Note { n: 1 })?).expect_err("gap must refuse");
    assert!(
        matches!(gap, RequestFoldError::Sequence(SequenceError::Gap { expected, actual }) if expected == Seq(2) && actual == Seq(3))
    );

    assert_eq!(requests.cursor(), Seq(1));
    assert_eq!(requests.find(None, &source).expect("request 1 still recorded").seq(), Seq(1));
    Ok(())
}
