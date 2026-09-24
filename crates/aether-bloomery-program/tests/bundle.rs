//! Bundle root: load by digest, invoke named programs, despawn the seq child.

use std::error::Error;
use std::fs;

use aether_bloomery_kinds::{
    BUNDLE_NAMESPACE, ClosureArtifact, EncodedArtifact, Invoke, Invoked, Mode, OpaqueBytes, ProgramName, Ref, Refusal,
    Utf8Text, artifact_digest,
};
use aether_bloomery_program::declarations;
use aether_component::{ComponentHostCapability, WasmTrampoline};
use aether_data::{Cites, Kind, LoadName, Storage};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::LoadComponent;
use wasmparser::{Parser, Payload};

const SECTION_NAME: &str = "aether.bloomery.programs";

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.result")]
struct SummarizeResult {
    text: Ref<Utf8Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.refuse.input")]
struct RefuseInput {
    marker: u32,
}

fn encoded<K: Storage + Clone + Cites>(value: &K) -> Result<EncodedArtifact, Box<dyn Error>> {
    Ok(EncodedArtifact::new(value)?)
}

fn closure_of<K: Storage + Clone + Cites>(value: &K) -> Result<ClosureArtifact, Box<dyn Error>> {
    let encoded = encoded(value)?;
    Ok(ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec()))
}

fn program_name(name: &str) -> ProgramName {
    ProgramName::new(name).expect("valid program name")
}

fn assert_fixture_section(wasm: &[u8]) {
    let decoded = declarations(&section_bytes(wasm)).expect("aether.bloomery.programs decodes");
    assert_eq!(decoded.len(), 6, "the custom section lists every exported program");
    let names: Vec<&str> = decoded.iter().map(|program| program.name.as_str()).collect();
    assert!(names.contains(&"test.program.summarize"), "{names:?}");
    assert!(names.contains(&"test.program.refuse"), "{names:?}");
    assert!(names.contains(&"test.program.fetch_body"), "{names:?}");
    assert!(names.contains(&"test.program.exec"), "{names:?}");
    assert!(names.contains(&"test.program.stall"), "{names:?}");
    assert!(names.contains(&"test.program.read_uncited"), "{names:?}");
    let summarize = decoded
        .iter()
        .find(|program| program.name.as_str() == "test.program.summarize")
        .expect("summarize declaration");
    assert_eq!(summarize.input, SummarizeInput::ID);
    assert_eq!(summarize.result, SummarizeResult::ID);
    assert_eq!(summarize.mode, Mode::Pure);
    let refuse =
        decoded.iter().find(|program| program.name.as_str() == "test.program.refuse").expect("refuse declaration");
    assert_eq!(refuse.input, RefuseInput::ID);
    assert_eq!(refuse.mode, Mode::Pure);
    let fetch_body = decoded
        .iter()
        .find(|program| program.name.as_str() == "test.program.fetch_body")
        .expect("fetch_body declaration");
    assert_eq!(fetch_body.mode, Mode::Sampled);
    let exec = decoded.iter().find(|program| program.name.as_str() == "test.program.exec").expect("exec declaration");
    assert_eq!(exec.mode, Mode::Sampled);
}

fn section_bytes(wasm: &[u8]) -> Vec<u8> {
    let mut section = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::CustomSection(reader) = payload.expect("parse program fixture wasm")
            && reader.name() == SECTION_NAME
        {
            section.extend_from_slice(reader.data());
        }
    }
    section
}

#[test]
fn bundle_root_invokes_named_programs_and_retires_the_seq_child() -> Result<(), Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let digest = artifact_digest(OpaqueBytes::ID, &wasm);
    let expected_name = format!("aether.component/aether.embedded:{digest}");

    assert_fixture_section(&wasm);

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (root, path) = harness
        .load_any(&LoadComponent {
            wasm,
            name: Some(digest.to_string()),
            config: Vec::new(),
            export: Some(BUNDLE_NAMESPACE.to_owned()),
        })
        .unwrap_or_else(|error| panic!("program fixture load failed: {error}"));
    assert_eq!(path.to_string(), expected_name, "root is named by the OpaqueBytes artifact digest");

    let text = "hello";
    let text_artifact = ClosureArtifact::new(Utf8Text::ID, text.as_bytes().to_vec());
    let input = SummarizeInput { text: Ref::of_text(text) };
    let input_artifact = closure_of(&input)?;
    let summarize = Invoke::new(
        1,
        program_name("test.program.summarize"),
        input_artifact.digest(),
        vec![text_artifact, input_artifact],
    );
    let summarized = harness
        .execute(vec![("summarize", HarnessOp::send_and_await_reply(root, &summarize))])
        .expect("summarize invoke");
    let Invoked::Completed { seq, result, staged } =
        summarized.reply::<Invoked>("summarize").expect("decode summarize Invoked")
    else {
        panic!("expected Completed");
    };
    assert_eq!(seq, 1);
    let expected = SummarizeResult { text: Ref::of_text("summary:hello") };
    let expected_encoded = encoded(&expected)?;
    assert_eq!(result, artifact_digest(SummarizeResult::ID, expected_encoded.bytes()));
    assert_eq!(result, expected_encoded.digest());
    assert_eq!(staged, vec![EncodedArtifact::text("summary:hello"), expected_encoded]);

    // The bundle root and its per-seq inline child are generated types the
    // test cannot name, so both are looked up as the host sees them: the root
    // is the trampoline keyed by its load name, and the seq child's alias —
    // keyed by its seq beneath the root — resolves to that trampoline's
    // endpoint.
    let host = harness.actor_ref::<ComponentHostCapability>();
    let trampoline = harness
        .child::<ComponentHostCapability, WasmTrampoline>(&host, LoadName::new(&digest.to_string())?)
        .expect("the bundle root is live");
    let orphan = harness.child::<WasmTrampoline, WasmTrampoline>(&trampoline, LoadName::new("1")?);
    assert!(orphan.is_err(), "a completed invocation must despawn its seq child; got {orphan:?}");

    let refuse_input = RefuseInput { marker: 1 };
    let refuse_artifact = closure_of(&refuse_input)?;
    let refuse = Invoke::new(2, program_name("test.program.refuse"), refuse_artifact.digest(), vec![refuse_artifact]);
    let refused =
        harness.execute(vec![("refuse", HarnessOp::send_and_await_reply(root, &refuse))]).expect("refuse invoke");
    match refused.reply::<Invoked>("refuse").expect("decode refuse Invoked") {
        Invoked::Refused { seq, refusal: Refusal::Refused { .. } } => assert_eq!(seq, 2),
        other => panic!("expected Refused, got {other:?}"),
    }

    let unknown = Invoke::new(3, program_name("test.program.missing"), summarize.input(), Vec::new());
    let rejected =
        harness.execute(vec![("unknown", HarnessOp::send_and_await_reply(root, &unknown))]).expect("unknown invoke");
    match rejected.reply::<Invoked>("unknown").expect("decode unknown Invoked") {
        Invoked::Rejected { seq, .. } => assert_eq!(seq, 3),
        other => panic!("expected Rejected, got {other:?}"),
    }

    Ok(())
}
