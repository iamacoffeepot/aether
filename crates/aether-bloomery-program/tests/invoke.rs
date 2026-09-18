//! Guest `invoke` owns decoding, staging, and the orphan rule.

use std::error::Error;

use aether_bloomery_kinds::{
    ClosureArtifact, Digest, EncodedArtifact, Invoke, Invoked, Mode, OpaqueBytes, ProgramName, Ref, Refusal, Utf8Text,
    artifact_digest,
};
use aether_bloomery_program::{Env, Program, Pure, invoke};
use aether_data::{Cites, Kind, Storage};

fn program_name<P: Program>() -> ProgramName {
    ProgramName::new(P::NAME).expect("valid program name")
}

fn encoded<K: Storage + Clone + Cites>(value: &K) -> Result<EncodedArtifact, Box<dyn Error>> {
    Ok(EncodedArtifact::new(value)?)
}

fn closure_of<K: Storage + Clone + Cites>(value: &K) -> Result<ClosureArtifact, Box<dyn Error>> {
    let encoded = encoded(value)?;
    Ok(ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec()))
}

fn send<P: Program>(input: Digest, closure: Vec<ClosureArtifact>) -> Invoked {
    invoke::<P>(Invoke::new(7, program_name::<P>(), input, closure))
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.invoke_child")]
struct Child {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.invoke_input")]
struct CiteInput {
    child: Ref<Child>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.invoke_result")]
struct CiteResult {
    text: Ref<Utf8Text>,
}

struct Cite;

impl Program for Cite {
    const NAME: &'static str = "cite.child";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read a closure child and stage cited text.";
    type Input = CiteInput;
    type Result = CiteResult;

    fn run(input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        let _child = env.read(input.child)?;
        Ok(CiteResult { text: env.stage_text("hello") })
    }
}

struct MissingInput;

impl Program for MissingInput {
    const NAME: &'static str = "missing.input";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Never runs; the input is absent.";
    type Input = Child;
    type Result = Child;

    fn run(input: Self::Input, _env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        Ok(input)
    }
}

struct MissingRead;

impl Program for MissingRead {
    const NAME: &'static str = "missing.read";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read a digest the closure does not carry.";
    type Input = Child;
    type Result = Child;

    fn run(_input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        env.read(Ref::<Child>::from_digest(Digest::from_bytes([9; 32])))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.invoke_pair")]
struct Pair {
    a: Ref<OpaqueBytes>,
    b: Ref<OpaqueBytes>,
}

struct Orphan;

impl Program for Orphan {
    const NAME: &'static str = "stage.orphan";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Stage a blob the result does not cite.";
    type Input = Child;
    type Result = Pair;

    fn run(_input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        let a = env.stage_bytes(b"one");
        let _orphan = env.stage_bytes(b"two");
        Ok(Pair { a, b: a })
    }
}

struct Dedupe;

impl Program for Dedupe {
    const NAME: &'static str = "stage.dedupe";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Stage the same bytes twice.";
    type Input = Child;
    type Result = Pair;

    fn run(_input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        let a = env.stage_bytes(b"same");
        let b = env.stage_bytes(b"same");
        Ok(Pair { a, b })
    }
}

#[test]
fn absent_input_is_input_missing() {
    match send::<MissingInput>(Digest::from_bytes([1; 32]), Vec::new()) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputMissing } => {}
        other => panic!("expected InputMissing, got {other:?}"),
    }
}

#[test]
fn wrong_kind_input_is_input_decode() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 1 };
    let artifact = closure_of(&child)?;
    match send::<Cite>(artifact.digest(), vec![artifact]) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputDecode } => Ok(()),
        other => panic!("expected InputDecode, got {other:?}"),
    }
}

#[test]
fn undecodable_input_is_input_decode() {
    let artifact = ClosureArtifact::new(Child::ID, vec![0xff, 0xff, 0xff]);
    match send::<MissingInput>(artifact.digest(), vec![artifact]) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputDecode } => {}
        other => panic!("expected InputDecode, got {other:?}"),
    }
}

#[test]
fn read_outside_the_closure_is_input_missing() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 1 };
    let artifact = closure_of(&child)?;
    match send::<MissingRead>(artifact.digest(), vec![artifact]) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputMissing } => Ok(()),
        other => panic!("expected InputMissing, got {other:?}"),
    }
}

#[test]
fn completed_reads_a_closure_child_and_stages_cited_text() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 4 };
    let child_artifact = closure_of(&child)?;
    let input = CiteInput { child: Ref::of_encoded(&child)? };
    let input_artifact = closure_of(&input)?;
    let Invoked::Completed { seq, result, staged } =
        send::<Cite>(input_artifact.digest(), vec![child_artifact, input_artifact])
    else {
        panic!("expected Completed");
    };
    assert_eq!(seq, 7);
    let expected = CiteResult { text: Ref::of_text("hello") };
    let expected_encoded = encoded(&expected)?;
    assert_eq!(result, artifact_digest(CiteResult::ID, expected_encoded.bytes()));
    assert_eq!(result, expected_encoded.digest());
    assert_eq!(staged, vec![EncodedArtifact::text("hello"), expected_encoded]);
    Ok(())
}

#[test]
fn unreachable_staged_blob_is_refused() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 1 };
    let artifact = closure_of(&child)?;
    match send::<Orphan>(artifact.digest(), vec![artifact]) {
        Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason } } => {
            assert!(reason.as_str().contains("is not reachable from the result"), "{}", reason.as_str());
            Ok(())
        }
        other => panic!("expected Refused orphan, got {other:?}"),
    }
}

#[test]
fn staging_the_same_bytes_twice_yields_one_encoded_artifact() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 1 };
    let artifact = closure_of(&child)?;
    let Invoked::Completed { staged, .. } = send::<Dedupe>(artifact.digest(), vec![artifact]) else {
        panic!("expected Completed");
    };
    let bytes = EncodedArtifact::opaque_bytes(b"same");
    let pair = encoded(&Pair { a: Ref::of_bytes(b"same"), b: Ref::of_bytes(b"same") })?;
    assert_eq!(staged, vec![bytes, pair]);
    Ok(())
}
