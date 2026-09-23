//! The summarize-caller rule answers a `test.program.summarize.input` move with a
//! `CallProgram` for the summarize program, and ignores moves of other kinds.

use std::error::Error;
use std::fs;

use aether_actor::Addressable;
use aether_bloomery_kinds::{
    BUNDLE_NAMESPACE, CallProgram, Digest, Evaluated, Event, Head, HeadMoved, JournalEntry, OpaqueBytes, ProgramName,
    Ref, Tree, artifact_digest,
};
use aether_component::ComponentHostCapability;
use aether_data::{Kind, Storage, StorageData};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::{SUMMARIZE_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};

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

fn load_root(harness: &mut SubstrateHarness, wasm: Vec<u8>) -> String {
    let digest = artifact_digest(OpaqueBytes::ID, &wasm).to_string();
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some(digest.clone()),
                    config: Vec::new(),
                    export: Some(BUNDLE_NAMESPACE.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { path: name, .. } => name.to_string(),
        LoadResult::Err { error } => panic!("load_component({digest}): {error}"),
    }
}

fn evaluated(harness: &mut SubstrateHarness, root: &str, event: &Event) -> Evaluated {
    harness
        .execute(vec![("event", HarnessOp::send_and_await_reply(root, event))])
        .expect("event sequence")
        .reply::<Evaluated>("event")
        .expect("decode Evaluated")
}

#[test]
fn call_summarize_returns_call_program_for_the_summarize_input_move() -> Result<(), Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_reactor_call") else {
        return Ok(());
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let root = load_root(&mut harness, wasm);

    // Catches a driver intent encoded through a codec the root or driver can't decode; the rule not
    // selected in the bundle; `input` not taken from the trigger; the wrong program head or name reaching #6210.
    let moved = moved_to(1, "inputs", Ref::<SummarizeInput>::from_digest(Digest::from_bytes([7; 32])));
    match evaluated(&mut harness, &root, &Event::new(moved)) {
        Evaluated::Completed { seq: 1, intents } => {
            assert_eq!(intents.len(), 1);
            let intent = &intents[0];
            assert_eq!(intent.reactor().as_str(), "test.bloomery.summarize.caller");
            assert_eq!(intent.rule().as_str(), "call_summarize");
            assert_eq!(intent.kind(), CallProgram::ID);
            assert_eq!(
                CallProgram::decode_from_bytes(intent.bytes()).expect("CallProgram mail decodes"),
                CallProgram {
                    program: SUMMARIZE_BUNDLE,
                    name: ProgramName::new(SUMMARIZE_PROGRAM)?,
                    input: Digest::from_bytes([7; 32]),
                }
            );
        }
        other => panic!("{other:?}"),
    }

    // Catches a trigger that fires on every head move: under #6210 that would call the program on the
    // reactor-set move itself.
    let other_kind = moved_to(2, "inputs", Ref::<Tree>::from_digest(Digest::from_bytes([9; 32])));
    match evaluated(&mut harness, &root, &Event::new(other_kind)) {
        Evaluated::Completed { seq: 2, intents } => assert!(intents.is_empty()),
        other => panic!("{other:?}"),
    }
    Ok(())
}
