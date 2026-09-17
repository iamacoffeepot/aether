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
use aether_test_fixtures_kinds::{
    ReactorGuardedPublication, ReactorOpenPublication, SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME,
};

#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

const VIEWS: &str = "test.bloomery.reactor";

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

fn load_cluster(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str) -> String {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let config = ClusterConfig { output: SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME.to_owned() };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some(name.to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some(VIEWS.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("load_component({name}): {error}"),
    }
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

/// Shared fold, successful guarded output, declined guard, and isolated clusters.
///
/// This fails if the two reactors re-fold Heads separately, if a declined
/// current-head guard still publishes, if two loaded clusters share an Owner,
/// or if mailing one cluster advances the other.
#[test]
fn reactor_bundle_shared_fold_guard_and_isolated_clusters() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let cluster_a = load_cluster(&mut harness, &wasm_path, "reactor-a");
    let cluster_b = load_cluster(&mut harness, &wasm_path, "reactor-b");

    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    assert_eq!(push(&mut harness, &cluster_a, vec![journal_moved(1, "current", program)]), 1);
    assert_eq!(harness.count_observed(ReactorGuardedPublication::NAME), 0);
    assert_eq!(harness.count_observed(ReactorOpenPublication::NAME), 0);

    assert_eq!(push(&mut harness, &cluster_b, vec![journal_moved(1, "source", tree)]), 1);
    assert_eq!(
        harness.count_observed(ReactorGuardedPublication::NAME),
        0,
        "B must not see A's current head; a shared owner would resolve the guard",
    );
    assert_eq!(harness.count_observed(ReactorOpenPublication::NAME), 1);

    assert_eq!(push(&mut harness, &cluster_a, vec![journal_moved(2, "source", tree)]), 2);
    assert_eq!(
        harness.count_observed(ReactorGuardedPublication::NAME),
        1,
        "both reactors share A's folded Heads, so the guarded arm emits once",
    );
    assert_eq!(
        harness.count_observed(ReactorOpenPublication::NAME),
        2,
        "A's unguarded peer emits on the same prepared prefix as the guarded peer",
    );

    let b_status = harness
        .execute(vec![("status-b", HarnessOp::send_and_await_reply(&cluster_b, &ClusterStatusQuery))])
        .expect("cluster B status")
        .reply::<ClusterStatus>("status-b")
        .expect("decode cluster B status");
    assert_eq!(b_status.cursor, 1, "mailing A must not advance B's views or routes");
}
