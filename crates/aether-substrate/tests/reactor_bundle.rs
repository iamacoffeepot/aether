//! Generated reactor-bundle WASM: shared views, guard decline, and isolated clusters.
//!
//! Loads the compiled `aether_test_fixtures_bundle` wasm. A skip when the
//! artifact is absent is not proof of this wiring.

use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_bloomery_kinds::{Digest, Head, Program, Ref, Tree};
use aether_bloomery_reactor::{ClusterConfig, ClusterStatus, ClusterStatusQuery, JournalEntry, PushEntries};
use aether_component::ComponentHostCapability;
use aether_data::{Kind, Storage, StorageData};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::{CollectReactorOutputs, ReactorOutputsReport};

const VIEWS: &str = "test.bloomery.reactor";
const SINK: &str = "test.bloomery.reactor.sink";

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn journal_moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> JournalEntry {
    let event = Head::<K>::new(name).move_to(to);
    JournalEntry {
        seq,
        kind: aether_bloomery_kinds::HeadMoved::<K>::NAME.to_owned(),
        cause: None,
        recorded_at_millis: 0,
        bytes: aether_bloomery_kinds::HeadMoved::<K>::encode_storage(&StorageData::from_value(event))
            .expect("storage encode"),
    }
}

fn load_export(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str, export: &str, config: Vec<u8>) -> String {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent { wasm, name: Some(name.to_owned()), config, export: Some(export.to_owned()) },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("load_component({name}): {error}"),
    }
}

fn load_cluster(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str, output: String) -> String {
    let config = ClusterConfig { output };
    load_export(harness, wasm_path, name, VIEWS, config.encode_into_bytes())
}

fn push(harness: &mut SubstrateHarness, address: &str, entries: Vec<JournalEntry>) -> u64 {
    harness
        .execute(vec![
            ("push", HarnessOp::send_and_settle(address, &PushEntries { entries })),
            ("status", HarnessOp::send_and_await_reply(address, &ClusterStatusQuery)),
        ])
        .expect("push sequence")
        .reply::<ClusterStatus>("status")
        .expect("decode ClusterStatus")
        .cursor
}

fn collect(harness: &mut SubstrateHarness, sink: &str) -> ReactorOutputsReport {
    harness
        .execute(vec![("collect", HarnessOp::send_and_await_reply(sink, &CollectReactorOutputs))])
        .expect("collect sequence")
        .reply::<ReactorOutputsReport>("collect")
        .expect("decode ReactorOutputsReport")
}

/// Shared fold, successful guarded output, declined guard, and isolated clusters.
///
/// This fails if the two reactors construct separate `FoldTally` instances, if a
/// declined current-head guard still publishes, if two loaded clusters share
/// views or output routes, or if mailing one cluster advances the other.
#[test]
fn reactor_bundle_shared_fold_guard_and_isolated_clusters() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let sink_a = load_export(&mut harness, &wasm_path, "sink-a", SINK, Vec::new());
    let sink_b = load_export(&mut harness, &wasm_path, "sink-b", SINK, Vec::new());
    let cluster_a = load_cluster(&mut harness, &wasm_path, "reactor-a", sink_a.clone());
    let cluster_b = load_cluster(&mut harness, &wasm_path, "reactor-b", sink_b.clone());

    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    assert_eq!(push(&mut harness, &cluster_a, vec![journal_moved(1, "current", program)]), 1);
    let after_current = collect(&mut harness, &sink_a);
    assert!(after_current.guarded.is_empty());
    assert!(after_current.open.is_empty());

    assert_eq!(push(&mut harness, &cluster_b, vec![journal_moved(1, "source", tree)]), 1);
    let from_b = collect(&mut harness, &sink_b);
    assert!(
        from_b.guarded.is_empty(),
        "B must not see A's current head; a shared owner would resolve the guard: {:?}",
        from_b.guarded
    );
    assert_eq!(from_b.open.len(), 1, "{:?}", from_b.open);
    assert_eq!(from_b.open[0].digest, [2; 32]);
    assert_eq!(from_b.open[0].folds, 1);
    let fold_b = from_b.open[0].fold_id;

    assert_eq!(push(&mut harness, &cluster_a, vec![journal_moved(2, "source", tree)]), 2);
    let from_a = collect(&mut harness, &sink_a);
    assert_eq!(from_a.guarded.len(), 1, "{:?}", from_a.guarded);
    assert_eq!(from_a.open.len(), 1, "{:?}", from_a.open);
    assert_eq!(from_a.guarded[0].digest, [2; 32]);
    assert_eq!(from_a.open[0].digest, [2; 32]);
    assert_eq!(from_a.guarded[0].folds, 2);
    assert_eq!(from_a.open[0].folds, 2);
    assert_eq!(
        from_a.guarded[0].fold_id, from_a.open[0].fold_id,
        "both reactors must consume the same FoldTally snapshot"
    );

    let still_b = collect(&mut harness, &sink_b);
    assert_eq!(still_b.open.len(), 1);
    assert!(still_b.guarded.is_empty());
    assert_eq!(still_b.open[0].fold_id, fold_b);

    let b_status = harness
        .execute(vec![("status-b", HarnessOp::send_and_await_reply(&cluster_b, &ClusterStatusQuery))])
        .expect("cluster B status")
        .reply::<ClusterStatus>("status-b")
        .expect("decode cluster B status");
    assert_eq!(b_status.cursor, 1, "mailing A must not advance B's views or routes");
}
