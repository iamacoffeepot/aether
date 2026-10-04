//! Bundle root: load by digest, invoke named programs, despawn the seq child.

use std::borrow::Cow;
use std::error::Error;
use std::fs;

use aether_bloomery_kinds::{
    BUNDLE_NAMESPACE, ClosureArtifact, EncodedArtifact, Invoke, Invoked, Mode, ProgramApi, ProgramName, ProgramRoot,
    Refusal,
};
use aether_bloomery_program::declarations;
use aether_component::ComponentHostCapability;
use aether_data::{Cites, Doc, DocNode, Kind, OpaqueBytes, Ref, Storage, Utf8Text, artifact_digest};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{ListComponents, ListComponentsResult, LoadComponent};
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
    let (kind, bytes, _) = encoded(value)?.into_parts();
    Ok(ClosureArtifact::new(kind, bytes))
}

fn program_name(name: &str) -> ProgramName {
    ProgramName::new(name).expect("valid program name")
}

fn assert_fixture_section(wasm: &[u8]) {
    let decoded = declarations(&section_bytes(wasm)).expect("aether.bloomery.programs decodes");
    assert_eq!(decoded.len(), 6, "the custom section lists every exported program");
    let names: Vec<&str> = decoded.iter().map(|declared| declared.program.name.as_str()).collect();
    assert!(names.contains(&"test.program.summarize"), "{names:?}");
    assert!(names.contains(&"test.program.refuse"), "{names:?}");
    assert!(names.contains(&"test.program.fetch_body"), "{names:?}");
    assert!(names.contains(&"test.program.stall"), "{names:?}");
    assert!(names.contains(&"test.program.read_uncited"), "{names:?}");
    assert!(names.contains(&"test.program.read_large"), "{names:?}");
    let find = |name: &str| decoded.iter().find(|declared| declared.program.name.as_str() == name);
    let summarize = find("test.program.summarize").expect("summarize declaration");
    assert_eq!(summarize.program.input, SummarizeInput::ID);
    assert_eq!(summarize.program.result, SummarizeResult::ID);
    assert_eq!(summarize.program.mode, Mode::Pure);
    assert!(summarize.apis.is_empty(), "summarize binds no API, got {:?}", summarize.apis);
    assert_eq!(summarize.doc, "Read cited text and stage a summary derived from it.");
    let DocNode::Struct { fields } = &summarize.input_docs else {
        panic!("summarize's input docs are a struct tree, got {:?}", summarize.input_docs);
    };
    assert_eq!(fields.len(), 1, "one doc per input field");
    assert_eq!(fields[0].doc, Doc::Written(Cow::Borrowed("The text to summarize.")));
    let refuse = find("test.program.refuse").expect("refuse declaration");
    assert_eq!(refuse.program.input, RefuseInput::ID);
    assert_eq!(refuse.program.mode, Mode::Pure);
    let fetch_body = find("test.program.fetch_body").expect("fetch_body declaration");
    assert_eq!(fetch_body.program.mode, Mode::Sampled);
    assert_eq!(fetch_body.apis, [ProgramApi::Http]);
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

    assert_fixture_section(&wasm);

    // No http capability is composed: the invocation declares no dependency, so the load needs none live.
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (root, path) = harness
        .load_any(&LoadComponent {
            wasm,
            name: Some(digest.to_string()),
            config: Vec::new(),
            export: Some(BUNDLE_NAMESPACE.to_owned()),
        })
        .unwrap_or_else(|error| panic!("program fixture load failed: {error}"));
    let (namespace, key) = path.as_str().split_once(':').expect("a bundle root is keyed");
    assert_eq!(key, digest.to_string(), "the root is keyed by its load name");
    let hash = namespace.strip_prefix(BUNDLE_NAMESPACE).and_then(|hash| hash.strip_prefix('.'));
    assert!(
        hash.is_some_and(|hash| hash.len() == 64),
        "the root publishes as {BUNDLE_NAMESPACE}.<module hash>: {namespace}"
    );
    let root = harness.cast::<ProgramRoot>(root)?;

    let text = "hello";
    let text_artifact = ClosureArtifact::new(Utf8Text::ID, text.as_bytes().to_vec());
    let input = SummarizeInput { text: Ref::of_text(text) };
    let input_artifact = closure_of(&input)?;
    let summarize = Invoke::new(
        1,
        program_name("test.program.summarize"),
        input_artifact.claimed().unverified(),
        vec![text_artifact, input_artifact],
    );
    let summarized = harness
        .execute(vec![("summarize", HarnessOp::send_and_await_reply(&root, &summarize))])
        .expect("summarize invoke");
    let Invoked::Completed { seq, result, staged } =
        summarized.reply::<Invoked>("summarize").expect("decode summarize Invoked")
    else {
        panic!("expected Completed");
    };
    assert_eq!(seq, 1);
    let expected = SummarizeResult { text: Ref::of_text("summary:hello") };
    let expected_encoded = encoded(&expected)?;
    assert_eq!(result, expected_encoded.digest());
    assert_eq!(staged, vec![EncodedArtifact::text("summary:hello"), expected_encoded]);

    // The per-seq inline child is a generated type the test cannot name, so
    // its absence is read as the host lists its guests: the child's alias,
    // keyed by its seq beneath the root the load reply named, stands no more.
    let seq_child = format!("{path}/{BUNDLE_NAMESPACE}.invocation:1");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let listed = harness
        .execute(vec![("list", HarnessOp::send_and_await_reply(&host, &ListComponents {}))])
        .expect("list components");
    let names = listed.reply::<ListComponentsResult>("list").expect("decode ListComponentsResult").names;
    assert!(names.contains(&path.to_string()), "the bundle root is listed: {names:?}");
    assert!(!names.contains(&seq_child), "a completed invocation must despawn its seq child; got {names:?}");

    let refuse_input = RefuseInput { marker: 1 };
    let refuse_artifact = closure_of(&refuse_input)?;
    let refuse = Invoke::new(
        2,
        program_name("test.program.refuse"),
        refuse_artifact.claimed().unverified(),
        vec![refuse_artifact],
    );
    let refused =
        harness.execute(vec![("refuse", HarnessOp::send_and_await_reply(&root, &refuse))]).expect("refuse invoke");
    match refused.reply::<Invoked>("refuse").expect("decode refuse Invoked") {
        Invoked::Refused { seq, refusal: Refusal::Refused { .. } } => assert_eq!(seq, 2),
        other => panic!("expected Refused, got {other:?}"),
    }

    let unknown = Invoke::new(3, program_name("test.program.missing"), summarize.input(), Vec::new());
    let rejected =
        harness.execute(vec![("unknown", HarnessOp::send_and_await_reply(&root, &unknown))]).expect("unknown invoke");
    match rejected.reply::<Invoked>("unknown").expect("decode unknown Invoked") {
        Invoked::Rejected { seq, .. } => assert_eq!(seq, 3),
        other => panic!("expected Rejected, got {other:?}"),
    }

    Ok(())
}
