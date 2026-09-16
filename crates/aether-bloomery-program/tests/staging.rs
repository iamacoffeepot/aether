//! `finish` refuses a staged blob that the result does not cite.

use std::error::Error;

use aether_bloomery_kinds::{Mode, OpaqueBytes, Ref};
use aether_bloomery_program::{FinishError, Program, Staging};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.stage_pair")]
struct Pair {
    a: Ref<OpaqueBytes>,
    b: Ref<OpaqueBytes>,
}

struct PairProg;

impl Program for PairProg {
    const NAME: &'static str = "stage.pair";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Cite two blobs from the result.";
    type Input = Pair;
    type Result = Pair;
}

#[test]
fn finish_refuses_an_orphan_then_accepts_when_both_are_cited() -> Result<(), Box<dyn Error>> {
    let mut staging = Staging::<PairProg>::new();
    let a = staging.stage_bytes(b"one");
    let b = staging.stage_bytes(b"two");
    match staging.finish(Pair { a, b: a }) {
        Err(FinishError::Orphaned { digest }) => assert_eq!(digest, b.digest()),
        Err(other) => panic!("expected Orphaned, got {other}"),
        Ok(_) => panic!("expected Orphaned, got Ok"),
    }

    let mut staging = Staging::<PairProg>::new();
    let a = staging.stage_bytes(b"one");
    let b = staging.stage_bytes(b"two");
    let execution = staging.finish(Pair { a, b })?;
    assert_eq!(execution.result().digest(), Ref::<Pair>::of_encoded(&Pair { a, b })?.digest());
    Ok(())
}
