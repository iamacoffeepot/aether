//! `Ran<P>`: one recorded run of a program, typed by the program.

use aether_bloomery_kinds::{Digest, Entry, Mode, ProgramName, ProgramRef, Seq, Transition};
use aether_bloomery_program::{Program, Ran};
use aether_data::{Citations, Cites, Kind, Storage, StorageData};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.ran.input")]
struct EchoInput {
    value: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.ran.result")]
struct EchoResult {
    value: u64,
}

struct Echo;

impl Program for Echo {
    const NAME: &'static str = "test.ran.echo";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Echo the input.";
    const DOC: &'static str = "Echo the input.";
    type Input = EchoInput;
    type Result = EchoResult;
}

/// A recorded transition of the program named `program`, input `[2; 32]`, result `[3; 32]`.
fn transition_entry(program: &str) -> Entry {
    let transition = Transition {
        program: ProgramRef::new(Digest::from_bytes([1; 32]), ProgramName::new(program).expect("program name")),
        input: Digest::from_bytes([2; 32]),
        result: Digest::from_bytes([3; 32]),
    };
    Entry {
        seq: Seq(2),
        kind: Transition::ID,
        cause: Some(Seq(1)),
        recorded_at_millis: 0,
        bytes: Transition::encode_storage(&StorageData::from_value(transition)).expect("storage encode"),
    }
}

#[test]
fn a_run_of_its_program_decodes_with_typed_citations() {
    // Catches a `Ran` that swaps or drops a digest, or cites them under the
    // wrong kinds, so the driver would deliver the wrong artifacts.
    let ran = transition_entry("test.ran.echo").decode::<Ran<Echo>>().expect("a run of echo");
    assert_eq!(ran.program().name().as_str(), "test.ran.echo");
    assert_eq!(ran.input().digest(), Digest::from_bytes([2; 32]));
    assert_eq!(ran.result().digest(), Digest::from_bytes([3; 32]));

    let mut sink = Citations::default();
    ran.cites(&mut sink);
    let cited: Vec<_> = sink.into_vec().into_iter().map(|citation| (citation.kind, citation.bytes)).collect();
    assert_eq!(cited, vec![(EchoInput::ID, vec![2; 32]), (EchoResult::ID, vec![3; 32])]);
}

#[test]
fn another_programs_run_declines_and_a_malformed_one_fails() {
    // Catches another program's run reported as a broken payload, which
    // would fail a `Ran<P>` rule on every other program's transition, or a
    // malformed transition silently declined.
    let other = transition_entry("test.ran.other").decode::<Ran<Echo>>().expect_err("another program's run");
    assert!(other.is_unmatched(), "{other:?}");

    let malformed = Entry { bytes: vec![0xff], ..transition_entry("test.ran.echo") };
    let broken = malformed.decode::<Ran<Echo>>().expect_err("a malformed transition");
    assert!(!broken.is_unmatched(), "{broken:?}");
}
