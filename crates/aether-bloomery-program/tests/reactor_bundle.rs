//! Generated reactor-bundle WASM: digest-loaded Warm/Event/StatusQuery.

use std::fs;
use std::path::{Path, PathBuf};

use aether_actor::ProtocolRef;
use aether_bloomery_kinds::{
    BUNDLE_NAMESPACE, Evaluated, Event, Head, HeadMoved, JournalEntry, Program, REACTORS_SECTION, ReactorRoot,
    SetHeads, Status, StatusQuery, Tree, Warm, WarmEntries, Warmed, reactor_declarations,
};
use aether_data::{Digest, ErasedActorPath, Kind, OpaqueBytes, Ref, Storage, StorageData, artifact_digest};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SendTarget, SubstrateHarness};
use aether_kinds::LoadComponent;
use aether_test_fixtures_kinds::REACTOR_FOLD_FAIL_KIND;
use wasmparser::{Parser, Payload};

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn journal_moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> JournalEntry {
    let event = Head::<K>::new(name).move_to(to);
    JournalEntry {
        seq,
        kind: HeadMoved::<K>::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: HeadMoved::<K>::encode_storage(&StorageData::from_value(event)).expect("storage encode"),
        cites: Vec::new(),
    }
}

fn fold_fail(seq: u64) -> JournalEntry {
    JournalEntry {
        seq,
        kind: REACTOR_FOLD_FAIL_KIND,
        cause: None,
        recorded_at_millis: 0,
        bytes: Vec::new(),
        cites: Vec::new(),
    }
}

fn load_root(harness: &mut SubstrateHarness, wasm_path: &Path) -> (String, ProtocolRef<ReactorRoot>, ErasedActorPath) {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let digest = artifact_digest(OpaqueBytes::ID, &wasm).to_string();
    let loaded = harness.load_any(&LoadComponent {
        wasm,
        name: Some(digest.clone()),
        config: Vec::new(),
        export: Some(BUNDLE_NAMESPACE.to_owned()),
    });
    let (root, path) = loaded.unwrap_or_else(|error| panic!("load_component({digest}): {error}"));
    let root = harness.cast::<ReactorRoot>(root).unwrap_or_else(|error| panic!("cast {path}: {error}"));
    (digest, root, path)
}

fn boot() -> Option<(SubstrateHarness, PathBuf)> {
    let wasm_path = require_wasm("aether_test_fixtures_reactor")?;
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm_path))
}

fn reply<K: Kind, M: Kind, I>(harness: &mut SubstrateHarness, root: impl SendTarget<M, I>, mail: &M, label: &str) -> K {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(root, mail))])
        .unwrap_or_else(|error| panic!("{label}: {error}"))
        .reply::<K>(label)
        .unwrap_or_else(|error| panic!("decode {label}: {error}"))
}

#[test]
fn reactor_root_loads_by_digest_and_answers_its_caller() {
    // Catches a root that isn't loadable by digest with empty config, or whose replies or intents don't decode over mail.
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let (digest, root, path) = load_root(&mut harness, &wasm_path);
    let (namespace, key) = path.as_str().split_once(':').expect("a bundle root is keyed");
    assert_eq!(key, digest, "the root is keyed by its load name");
    let hash = namespace.strip_prefix(BUNDLE_NAMESPACE).and_then(|hash| hash.strip_prefix('.'));
    assert!(
        hash.is_some_and(|hash| hash.len() == 64),
        "the root publishes as {BUNDLE_NAMESPACE}.<module hash>: {namespace}"
    );

    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    let warmed: Warmed = reply(
        &mut harness,
        &root,
        &Warm::new(WarmEntries::new(vec![journal_moved(1, "current", program)]).expect("dense"), Vec::new())
            .expect("no artifacts"),
        "warm",
    );
    assert!(matches!(warmed, Warmed::Folded { through: 1 }), "{warmed:?}");

    let live: Evaluated =
        reply(&mut harness, &root, &Event::new(journal_moved(2, "source", tree), Vec::new()), "event");
    match live {
        Evaluated::Completed { seq: 2, intents } => {
            assert_eq!(intents.len(), 2);
            let guarded = SetHeads::decode_from_bytes(intents[0].bytes()).expect("guarded");
            let open = SetHeads::decode_from_bytes(intents[1].bytes()).expect("open");
            assert_eq!(guarded.changes().len(), 2);
            assert_eq!(guarded.changes()[0].head().as_str(), "published");
            assert_eq!(guarded.changes()[1].head().as_str(), "mirrored");
            assert!(guarded.changes().iter().all(|change| change.to() == Digest::from_bytes([2; 32])));
            assert_eq!(open.changes().len(), 1);
            assert_eq!(open.changes()[0].head().as_str(), "published");
            assert_eq!(open.changes()[0].to(), Digest::from_bytes([2; 32]));
            assert_eq!(intents[0].kind(), SetHeads::ID);
            assert_eq!(intents[1].kind(), SetHeads::ID);
            assert_eq!(intents[0].reactor().as_str(), "test.bloomery.source.publisher");
            assert_eq!(intents[1].reactor().as_str(), "test.bloomery.source.witness");
        }
        other => panic!("{other:?}"),
    }

    let other: Evaluated =
        reply(&mut harness, &root, &Event::new(journal_moved(3, "other", tree), Vec::new()), "other");
    assert!(matches!(other, Evaluated::Completed { seq: 3, ref intents } if intents.len() == 2), "{other:?}");

    let repeated: Evaluated =
        reply(&mut harness, &root, &Event::new(journal_moved(4, "source", tree), Vec::new()), "repeat");
    match repeated {
        Evaluated::Completed { seq: 4, intents } => {
            assert_eq!(intents.len(), 1, "the keyed aggregate reports source has moved twice");
            assert_eq!(intents[0].reactor().as_str(), "test.bloomery.source.witness");
        }
        other => panic!("{other:?}"),
    }

    let declined: Evaluated = {
        let wasm_path = require_wasm("aether_test_fixtures_reactor").expect("wasm");
        let mut second = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
        let (_, declined, _) = load_root(&mut second, &wasm_path);
        reply(&mut second, &declined, &Event::new(journal_moved(1, "source", tree), Vec::new()), "decline")
    };
    match declined {
        Evaluated::Completed { seq: 1, intents } => {
            assert_eq!(intents.len(), 1);
            assert!(SetHeads::decode_from_bytes(intents[0].bytes()).is_some());
            assert_eq!(intents[0].reactor().as_str(), "test.bloomery.source.witness");
        }
        other => panic!("{other:?}"),
    }

    let gap: Evaluated = reply(&mut harness, &root, &Event::new(journal_moved(9, "source", tree), Vec::new()), "gap");
    assert!(matches!(gap, Evaluated::OutOfSequence { seq: 9, expected: 5 }), "{gap:?}");

    let poisoned: Evaluated = reply(&mut harness, &root, &Event::new(fold_fail(5), Vec::new()), "poison");
    assert!(matches!(poisoned, Evaluated::Poisoned { seq: 5, last_trusted: 4, .. }), "{poisoned:?}");
    let status: Status = reply(&mut harness, &root, &StatusQuery, "status");
    assert!(status.poisoned());
    assert_eq!(status.cursor(), 4);
}

fn section_bytes(wasm: &[u8]) -> Vec<u8> {
    let mut section = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::CustomSection(reader) = payload.expect("parse reactor fixture wasm")
            && reader.name() == REACTORS_SECTION
        {
            section.extend_from_slice(reader.data());
        }
    }
    section
}

#[test]
fn bundle_section_declares_both_reactors_with_their_rule_kinds() {
    // Catches the bundle not emitting, emitting for the generated root, or recording the wrong type's id.
    let Some(wasm_path) = require_wasm("aether_test_fixtures_reactor") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let decoded = reactor_declarations(&section_bytes(&wasm)).expect("aether.bloomery.reactors decodes");
    assert_eq!(decoded.len(), 2, "the custom section lists both reactors");
    let publisher = decoded
        .iter()
        .find(|declaration| declaration.name().as_str() == "test.bloomery.source.publisher")
        .expect("publisher declaration");
    assert_eq!(publisher.rules().len(), 1);
    assert_eq!(publisher.rules()[0].name().as_str(), "publish_source");
    assert_eq!(publisher.rules()[0].trigger(), HeadMoved::<Tree>::ID);
    assert_eq!(publisher.rules()[0].output(), SetHeads::ID);
    let witness = decoded
        .iter()
        .find(|declaration| declaration.name().as_str() == "test.bloomery.source.witness")
        .expect("witness declaration");
    assert_eq!(witness.rules().len(), 1);
    assert_eq!(witness.rules()[0].name().as_str(), "note_heads");
    assert_eq!(witness.rules()[0].trigger(), HeadMoved::<Tree>::ID);
    assert_eq!(witness.rules()[0].output(), SetHeads::ID);
}
