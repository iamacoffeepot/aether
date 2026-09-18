//! Refusal and panic become faults; a wrong input kind is refused before any attempt.

mod common;

use std::error::Error;

use aether_bloomery_journal::{Batch, Journal, Seq};
use aether_bloomery_kinds::{ExecutorName, Fault, FaultReason, Utf8Text};
use aether_bloomery_program::{
    Applied, ApplyError, Execute, Execution, Executors, ReadArtifacts, Refusal, apply, declaration, digest,
};
use aether_data::Kind;
use common::{FixedClock, Trim, TrimInput};

struct RefuseExecutor;

impl Execute<Trim> for RefuseExecutor {
    fn execute(&self, _input: TrimInput, _store: &dyn ReadArtifacts) -> Result<Execution<Trim>, Refusal> {
        Err(Refusal::Refused("no".into()))
    }
}

struct PanicExecutor;

impl Execute<Trim> for PanicExecutor {
    fn execute(&self, _input: TrimInput, _store: &dyn ReadArtifacts) -> Result<Execution<Trim>, Refusal> {
        panic!("trim panicked");
    }
}

fn journal_with_input() -> Result<(Journal, aether_bloomery_kinds::Digest), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(1)))?;
    let mut batch = Batch::new();
    let empty = aether_bloomery_kinds::Tree::empty();
    let tree = batch.stage_encoded(&empty)?;
    let input = batch.stage_encoded(&TrimInput { tree })?;
    batch.stage_encoded(&declaration::<Trim>())?;
    journal.append(Seq(0), &batch)?;
    Ok((journal, input.digest()))
}

#[test]
fn refusal_and_panic_each_write_one_fault_and_no_extra_artifacts() -> Result<(), Box<dyn Error>> {
    let (mut journal, input) = journal_with_input()?;
    let mut executors = Executors::new();
    executors.register::<Trim, _>(ExecutorName::new("refuse")?, RefuseExecutor);
    let before = journal.head()?;
    let applied = apply(&mut journal, &executors, digest::<Trim>(), input, None)?;
    let Applied::Fault(seq) = applied else {
        panic!("expected Fault, got {applied:?}");
    };
    let entries = journal.read(before, 16)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].seq, seq);
    assert_eq!(entries[0].kind, Fault::ID);
    let fault = Journal::decode::<Fault>(&entries[0])?;
    assert!(matches!(fault.reason, FaultReason::Refused { .. }));
    assert_eq!(journal.head()?, Seq(before.0 + 1));

    let (mut journal, input) = journal_with_input()?;
    let mut executors = Executors::new();
    executors.register::<Trim, _>(ExecutorName::new("panic")?, PanicExecutor);
    let before = journal.head()?;
    let applied = apply(&mut journal, &executors, digest::<Trim>(), input, None)?;
    let Applied::Fault(_) = applied else {
        panic!("expected Fault, got {applied:?}");
    };
    let entries = journal.read(before, 16)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, Fault::ID);
    let fault = Journal::decode::<Fault>(&entries[0])?;
    match fault.reason {
        FaultReason::Panicked { message } => assert!(message.as_str().contains("trim panicked")),
        other => panic!("expected Panicked, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(before.0 + 1));
    Ok(())
}

#[test]
fn wrong_input_kind_is_refused_before_any_attempt() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(1)))?;
    let mut batch = Batch::new();
    let text = batch.stage_text("not a trim input");
    batch.stage_encoded(&declaration::<Trim>())?;
    journal.append(Seq(0), &batch)?;
    let before = journal.head()?;

    let mut executors = Executors::new();
    executors.register::<Trim, _>(ExecutorName::new("in_process")?, common::TrimExecutor);
    let error =
        apply(&mut journal, &executors, digest::<Trim>(), text.digest(), None).expect_err("wrong kind must fail");
    match error {
        ApplyError::InputKind { expected, actual } => {
            assert_eq!(expected, TrimInput::ID);
            assert_eq!(actual, Utf8Text::ID);
        }
        other => panic!("expected InputKind, got {other:?}"),
    }
    assert_eq!(journal.head()?, before);
    Ok(())
}

#[test]
fn a_missing_input_is_a_fault_and_writes_nothing_else() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(1)))?;
    let mut batch = Batch::new();
    batch.stage_encoded(&declaration::<Trim>())?;
    journal.append(Seq(0), &batch)?;
    let before = journal.head()?;
    let missing = aether_bloomery_kinds::Digest::from_bytes([9; 32]);

    let mut executors = Executors::new();
    executors.register::<Trim, _>(ExecutorName::new("in_process")?, common::TrimExecutor);
    let applied = apply(&mut journal, &executors, digest::<Trim>(), missing, None)?;
    let Applied::Fault(_) = applied else {
        panic!("expected Fault, got {applied:?}");
    };
    let entries = journal.read(before, 16)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, Fault::ID);
    let fault = Journal::decode::<Fault>(&entries[0])?;
    assert_eq!(fault.reason, FaultReason::InputMissing);
    assert_eq!(journal.head()?, Seq(before.0 + 1));
    Ok(())
}
