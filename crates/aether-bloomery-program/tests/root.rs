//! Native `Root` state machine: admission, attribution, dispatch.

use aether_bloomery_kinds::{ClosureArtifact, Digest, EncodedArtifact, Invoke, Invoked, Mode, ProgramName, Refusal};
use aether_bloomery_program::{
    __macro_internals, Env, Program, ProgramEntry, ProgramTable, Root, Sync, SyncProgram, dispatch,
};
use aether_data::MailboxId;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.root.count")]
struct Count {
    n: u32,
}

struct First;

impl Program for First {
    const NAME: &'static str = "test.root.first";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Echo the count.";
    type Input = Count;
    type Result = Count;
}

impl SyncProgram for First {
    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(input)
    }
}

struct Second;

impl Program for Second {
    const NAME: &'static str = "test.root.second";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Bump the count.";
    type Input = Count;
    type Result = Count;
}

impl SyncProgram for Second {
    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(Count { n: input.n.saturating_add(1) })
    }
}

static TABLE: ProgramTable =
    __macro_internals::program_table(&[ProgramEntry::of::<First>(), ProgramEntry::of::<Second>()]);

fn invoke_named(seq: u64, name: &str) -> Invoke {
    Invoke::new(seq, ProgramName::new(name).expect("valid program name"), Digest::from_bytes([0; 32]), Vec::new())
}

fn completed(seq: u64) -> Invoked {
    Invoked::Completed { seq, result: Digest::from_bytes([0; 32]), staged: Vec::new() }
}

#[test]
fn unknown_program_rejection_records_nothing() {
    // Catches recording a seq before the name check.
    let mut root: Root<u32> = Root::new(&TABLE);
    match root.admit(&invoke_named(3, "test.root.missing")) {
        Err(Invoked::Rejected { seq: 3, reason }) => assert_eq!(reason.as_str(), "unknown program"),
        Err(other) => panic!("expected an unknown-program rejection, got {other:?}"),
        Ok(_) => panic!("expected an unknown-program rejection, admitted"),
    }
    assert!(root.admit(&invoke_named(3, First::NAME)).is_ok(), "the rejected seq stays free");
}

#[test]
fn started_seq_rejects_a_second_admit_until_finished() {
    // Catches a second start silently overwriting a live child.
    let mut root: Root<u32> = Root::new(&TABLE);
    root.admit(&invoke_named(1, First::NAME)).expect("first admit").start(MailboxId(7), 11);
    match root.admit(&invoke_named(1, Second::NAME)) {
        Err(Invoked::Rejected { seq: 1, reason }) => assert_eq!(reason.as_str(), "seq already live"),
        Err(other) => panic!("expected a seq-live rejection, got {other:?}"),
        Ok(_) => panic!("expected a seq-live rejection, admitted twice"),
    }
    match root.finish(&completed(1), Some(MailboxId(7))) {
        Some((child, handle)) => {
            assert_eq!(child.0, 7);
            assert_eq!(handle, 11);
        }
        None => panic!("expected the child's reply to finish seq 1"),
    }
    assert!(root.admit(&invoke_named(1, Second::NAME)).is_ok(), "a finished seq admits again");
}

#[test]
fn spawn_failed_leaves_the_seq_free() {
    // Catches recording a seq whose child never spawned.
    let mut root: Root<u32> = Root::new(&TABLE);
    let admission = root.admit(&invoke_named(2, First::NAME)).expect("admit");
    match admission.spawn_failed() {
        Invoked::Rejected { seq: 2, reason } => assert_eq!(reason.as_str(), "failed to spawn invocation"),
        other => panic!("expected a spawn-failure rejection, got {other:?}"),
    }
    assert!(root.admit(&invoke_named(2, First::NAME)).is_ok(), "the failed seq stays free");
}

#[test]
fn foreign_source_keeps_the_seq_live() {
    // Catches a mis-attributed reply retiring the invocation.
    let mut root: Root<u32> = Root::new(&TABLE);
    root.admit(&invoke_named(4, First::NAME)).expect("admit").start(MailboxId(7), 11);
    let reply = Invoked::Refused { seq: 4, refusal: Refusal::InputMissing };
    assert!(root.finish(&reply, Some(MailboxId(9))).is_none(), "a foreign source finishes nothing");
    match root.finish(&reply, Some(MailboxId(7))) {
        Some((child, handle)) => {
            assert_eq!(child.0, 7);
            assert_eq!(handle, 11);
        }
        None => panic!("expected the child's reply to finish seq 4"),
    }
}

#[test]
fn sourceless_reply_finishes() {
    // Catches tightening today's sourceless acceptance.
    let mut root: Root<u32> = Root::new(&TABLE);
    root.admit(&invoke_named(5, Second::NAME)).expect("admit").start(MailboxId(7), 11);
    match root.finish(&completed(5), None) {
        Some((child, handle)) => {
            assert_eq!(child.0, 7);
            assert_eq!(handle, 11);
        }
        None => panic!("expected a sourceless reply to finish seq 5"),
    }
}

#[test]
fn stray_seq_finishes_nothing() {
    // Catches a stray reply disturbing the live table.
    let mut root: Root<u32> = Root::new(&TABLE);
    root.admit(&invoke_named(6, First::NAME)).expect("admit").start(MailboxId(7), 11);
    root.admit(&invoke_named(8, Second::NAME)).expect("admit").start(MailboxId(9), 13);
    assert!(root.finish(&completed(7), Some(MailboxId(7))).is_none(), "a stray seq finishes nothing");
    assert!(root.finish(&completed(6), Some(MailboxId(7))).is_some(), "seq 6 stays live");
    assert!(root.finish(&completed(8), Some(MailboxId(9))).is_some(), "seq 8 stays live");
}

#[test]
fn dispatch_runs_the_named_entry() {
    // Catches fall-through to the first entry or a missing rejection.
    let encoded = EncodedArtifact::new(&Count { n: 4 }).expect("encode input");
    let closure = ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec());
    let invoke = Invoke::new(
        9,
        ProgramName::new(Second::NAME).expect("valid name"),
        closure.claimed().unverified(),
        vec![closure],
    );
    match dispatch(&TABLE, invoke) {
        Invoked::Completed { seq: 9, result, .. } => {
            let expected = EncodedArtifact::new(&Count { n: 5 }).expect("encode result");
            assert_eq!(result, expected.digest());
        }
        other => panic!("expected Second to complete, got {other:?}"),
    }
    match dispatch(&TABLE, invoke_named(10, "test.root.missing")) {
        Invoked::Rejected { seq: 10, reason } => assert_eq!(reason.as_str(), "unknown program"),
        other => panic!("expected an unknown-program rejection, got {other:?}"),
    }
}
