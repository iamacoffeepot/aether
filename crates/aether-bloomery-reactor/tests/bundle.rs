//! Generated reactor-bundle WASM: digest-loaded Warm/Event/StatusQuery.

use std::fs;
use std::path::{Path, PathBuf};

use aether_actor::Addressable;
use aether_bloomery_kinds::{
    Digest, Evaluated, Event, Head, JournalEntry, OpaqueBytes, Program, Ref, Status, StatusQuery, Tree, Warm,
    WarmEntries, Warmed, artifact_digest,
};
use aether_bloomery_reactor::REACTOR_NAMESPACE;
use aether_component::ComponentHostCapability;
use aether_data::{Kind, Storage, StorageData};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::{REACTOR_FOLD_FAIL_KIND, ReactorGuardedPublication, ReactorOpenPublication};

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn journal_moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> JournalEntry {
    let event = Head::<K>::new(name).move_to(to);
    JournalEntry {
        seq,
        kind: aether_bloomery_kinds::HeadMoved::<K>::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: aether_bloomery_kinds::HeadMoved::<K>::encode_storage(&StorageData::from_value(event))
            .expect("storage encode"),
    }
}

fn fold_fail(seq: u64) -> JournalEntry {
    JournalEntry { seq, kind: REACTOR_FOLD_FAIL_KIND, cause: None, recorded_at_millis: 0, bytes: Vec::new() }
}

fn load_root(harness: &mut SubstrateHarness, wasm_path: &Path) -> (String, String) {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
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
                    export: Some(REACTOR_NAMESPACE.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    let name = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("load_component({digest}): {error}"),
    };
    (digest, name)
}

fn boot() -> Option<(SubstrateHarness, PathBuf)> {
    let wasm_path = require_wasm("aether_test_fixtures_reactor")?;
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm_path))
}

fn reply<K: Kind>(harness: &mut SubstrateHarness, address: &str, mail: &impl Kind, label: &str) -> K {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(address, mail))])
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
    let (digest, address) = load_root(&mut harness, &wasm_path);
    assert_eq!(address, format!("aether.component/aether.embedded:{digest}"));

    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    let warmed = reply::<Warmed>(
        &mut harness,
        &address,
        &Warm::new(WarmEntries::new(vec![journal_moved(1, "current", program)]).expect("dense")),
        "warm",
    );
    assert!(matches!(warmed, Warmed::Folded { through: 1 }), "{warmed:?}");

    let live = reply::<Evaluated>(&mut harness, &address, &Event::new(journal_moved(2, "source", tree)), "event");
    match live {
        Evaluated::Completed { seq: 2, intents } => {
            assert_eq!(intents.len(), 2);
            let guarded = ReactorGuardedPublication::decode_from_bytes(intents[0].bytes()).expect("guarded");
            let open = ReactorOpenPublication::decode_from_bytes(intents[1].bytes()).expect("open");
            assert_eq!(guarded.digest, [2; 32]);
            assert_eq!(open.digest, [2; 32]);
            assert_eq!(guarded.fold_id, open.fold_id);
            assert_eq!(intents[0].reactor().as_str(), "test.bloomery.source.publisher");
            assert_eq!(intents[1].reactor().as_str(), "test.bloomery.source.witness");
        }
        other => panic!("{other:?}"),
    }

    let declined = {
        let wasm_path = require_wasm("aether_test_fixtures_reactor").expect("wasm");
        let mut second = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
        let (_, declined_addr) = load_root(&mut second, &wasm_path);
        reply::<Evaluated>(&mut second, &declined_addr, &Event::new(journal_moved(1, "source", tree)), "decline")
    };
    match declined {
        Evaluated::Completed { seq: 1, intents } => {
            assert_eq!(intents.len(), 1);
            assert!(ReactorOpenPublication::decode_from_bytes(intents[0].bytes()).is_some());
            assert_eq!(intents[0].reactor().as_str(), "test.bloomery.source.witness");
        }
        other => panic!("{other:?}"),
    }

    let gap = reply::<Evaluated>(&mut harness, &address, &Event::new(journal_moved(9, "source", tree)), "gap");
    assert!(matches!(gap, Evaluated::OutOfSequence { seq: 9, expected: 3 }), "{gap:?}");

    let poisoned = reply::<Evaluated>(&mut harness, &address, &Event::new(fold_fail(3)), "poison");
    assert!(matches!(poisoned, Evaluated::Poisoned { seq: 3, last_trusted: 2, .. }), "{poisoned:?}");
    let status = reply::<Status>(&mut harness, &address, &StatusQuery, "status");
    assert!(status.poisoned());
    assert_eq!(status.cursor(), 2);
}
