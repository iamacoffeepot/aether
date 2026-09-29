//! One load of the mixed bundle answers program and reactor mail from one root.

use std::error::Error;
use std::fs;

use aether_actor::{ProtocolRef, Undeclared};
use aether_bloomery_kinds::{
    BUNDLE_NAMESPACE, CallInput, CallProgram, ClosureArtifact, Digest, EncodedArtifact, Evaluated, Event, Head,
    HeadMoved, Invoke, Invoked, JournalEntry, OpaqueBytes, PROGRAMS_SECTION, ProgramName, REACTORS_SECTION, Ref,
    Status, StatusQuery, Tree, Utf8Text, Warm, WarmEntries, Warmed, artifact_digest, reactor_declarations,
};
use aether_bloomery_program::declarations;
use aether_data::{Cites, ErasedActorPath, Kind, Storage, StorageData};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SendTarget, SubstrateHarness};
use aether_kinds::LoadComponent;
use aether_test_fixtures_kinds::{MIXED_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};
use wasmparser::{Parser, Payload};

fn moved_to<K: Kind + 'static>(seq: u64, head: &'static str, to: Ref<K>) -> JournalEntry {
    let event = Head::<K>::new(head).move_to(to);
    JournalEntry {
        seq,
        kind: HeadMoved::<K>::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: HeadMoved::<K>::encode_storage(&StorageData::from_value(event)).expect("storage encode"),
    }
}

fn encoded<K: Storage + Clone + Cites>(value: &K) -> Result<EncodedArtifact, Box<dyn Error>> {
    Ok(EncodedArtifact::new(value)?)
}

fn closure_of<K: Storage + Clone + Cites>(value: &K) -> Result<ClosureArtifact, Box<dyn Error>> {
    let (kind, bytes, _) = encoded(value)?.into_parts();
    Ok(ClosureArtifact::new(kind, bytes))
}

fn section_bytes(wasm: &[u8], name: &str) -> Vec<u8> {
    let mut section = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::CustomSection(reader) = payload.expect("parse mixed fixture wasm")
            && reader.name() == name
        {
            section.extend_from_slice(reader.data());
        }
    }
    section
}

/// The mixed root's program and reactor rows, as the test names a root type it cannot type.
#[aether_actor::protocol]
trait MixedRoot {
    fn invoke(mail: Invoke) -> Invoked;
    fn warm(mail: Warm) -> Undeclared;
    fn event(mail: Event) -> Undeclared;
    fn status(mail: StatusQuery) -> Status;
}

fn load_root(harness: &mut SubstrateHarness, wasm: Vec<u8>, digest: &str) -> (ProtocolRef<MixedRoot>, ErasedActorPath) {
    let loaded = harness.load_any(&LoadComponent {
        wasm,
        name: Some(digest.to_owned()),
        config: Vec::new(),
        export: Some(BUNDLE_NAMESPACE.to_owned()),
    });
    let (root, path) = loaded.unwrap_or_else(|error| panic!("load_component({digest}): {error}"));
    let root = harness.cast::<MixedRoot>(root).unwrap_or_else(|error| panic!("cast {path}: {error}"));
    (root, path)
}

fn reply<K: Kind, M: Kind, I>(harness: &mut SubstrateHarness, root: impl SendTarget<M, I>, mail: &M, label: &str) -> K {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(root, mail))])
        .unwrap_or_else(|error| panic!("{label}: {error}"))
        .reply::<K>(label)
        .unwrap_or_else(|error| panic!("decode {label}: {error}"))
}

#[test]
fn one_load_answers_program_and_reactor_mail() -> Result<(), Box<dyn Error>> {
    // Catches a generator that drops one role's section, state, or handlers from a mixed module, or a root whose reactor traffic disturbs its program table: one load must serve both roles.
    let Some(wasm_path) = require_wasm("aether_test_fixtures_mixed_bundle") else {
        return Ok(());
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let digest = artifact_digest(OpaqueBytes::ID, &wasm).to_string();

    let programs = declarations(&section_bytes(&wasm, PROGRAMS_SECTION)).expect("programs section decodes");
    assert_eq!(programs.len(), 1, "the programs section lists the one program");
    assert_eq!(programs[0].program.name.as_str(), SUMMARIZE_PROGRAM);
    let reactors = reactor_declarations(&section_bytes(&wasm, REACTORS_SECTION)).expect("reactors section decodes");
    assert_eq!(reactors.len(), 1, "the reactors section lists the one reactor");
    assert_eq!(reactors[0].name().as_str(), "test.bloomery.mixed.caller");

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (root, path) = load_root(&mut harness, wasm, &digest);
    let (namespace, key) = path.as_str().split_once(':').expect("a bundle root is keyed");
    assert_eq!(key, digest, "the root is keyed by its load name");
    let hash = namespace.strip_prefix(BUNDLE_NAMESPACE).and_then(|hash| hash.strip_prefix('.'));
    assert!(
        hash.is_some_and(|hash| hash.len() == 64),
        "the root publishes as {BUNDLE_NAMESPACE}.<module hash>: {namespace}"
    );

    let text_artifact = ClosureArtifact::new(Utf8Text::ID, b"hello".to_vec());
    let input = SummarizeInput { text: Ref::of_text("hello") };
    let input_artifact = closure_of(&input)?;
    let invoke = Invoke::new(
        1,
        ProgramName::new(SUMMARIZE_PROGRAM)?,
        input_artifact.claimed().unverified(),
        vec![text_artifact, input_artifact],
    );
    let invoked: Invoked = reply(&mut harness, &root, &invoke, "invoke-one");
    match invoked {
        Invoked::Completed { seq: 1, .. } => {}
        other => panic!("expected Completed seq 1, got {other:?}"),
    }

    let tree = Ref::<Tree>::from_digest(Digest::from_bytes([2; 32]));
    let warmup = Warm::new(WarmEntries::new(vec![moved_to(1, "current", tree)]).expect("dense"));
    let warmed: Warmed = reply(&mut harness, &root, &warmup, "warm");
    match warmed {
        Warmed::Folded { through: 1 } => {}
        other => panic!("expected Folded through 1, got {other:?}"),
    }

    let moved = moved_to(2, "inputs", Ref::<SummarizeInput>::from_digest(Digest::from_bytes([7; 32])));
    let evaluated: Evaluated = reply(&mut harness, &root, &Event::new(moved), "event");
    match evaluated {
        Evaluated::Completed { seq: 2, intents } => {
            assert_eq!(intents.len(), 1);
            let intent = &intents[0];
            assert_eq!(intent.reactor().as_str(), "test.bloomery.mixed.caller");
            assert_eq!(
                CallProgram::decode_from_bytes(intent.bytes()).expect("CallProgram mail decodes"),
                CallProgram {
                    program: MIXED_BUNDLE,
                    name: ProgramName::new(SUMMARIZE_PROGRAM)?,
                    input: CallInput::Stored(Digest::from_bytes([7; 32])),
                }
            );
        }
        other => panic!("{other:?}"),
    }

    let status: Status = reply(&mut harness, &root, &StatusQuery, "status");
    assert_eq!(status, Status::new(2, false));

    let second_artifact = closure_of(&input)?;
    let second = Invoke::new(
        2,
        ProgramName::new(SUMMARIZE_PROGRAM)?,
        second_artifact.claimed().unverified(),
        vec![ClosureArtifact::new(Utf8Text::ID, b"hello".to_vec()), second_artifact],
    );
    let invoked: Invoked = reply(&mut harness, &root, &second, "invoke-two");
    match invoked {
        Invoked::Completed { seq: 2, .. } => {}
        other => panic!("expected Completed seq 2, got {other:?}"),
    }
    Ok(())
}
