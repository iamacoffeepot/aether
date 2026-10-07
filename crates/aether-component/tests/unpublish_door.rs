//! Issue 7613: `Unpublish` withdraws one published namespace (ADR-0250 §5).
//! Refused while any instance of the namespace is live, naming it; once the
//! last instance is dropped the withdrawal answers `Ok`, a further spawn is
//! refused as unpublished, and the module's sibling namespace still spawns.
//! Publishing the same bytes afterwards binds them again.
//!
//! The fixtures are the #7109 group pair: `test.republish.gate` (instanced)
//! and `test.republish.peer` (a root singleton). Skipped when the fixture
//! wasm hasn't been built (`require_wasm`); CI pre-builds it and sets
//! `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a hard panic there.

use std::fs;

use aether_actor::HandlesKind;
use aether_component::ComponentHostCapability;
use aether_data::{Blob, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{
    DescribeComponent, DescribeComponentResult, DropComponent, DropResult, ListComponents, ListComponentsResult,
    Publish, PublishResult, Spawn, SpawnResult, Unpublish, UnpublishResult,
};

const GATE: &str = "test.republish.gate";
const PEER: &str = "test.republish.peer";

fn read(stem: &str) -> Option<Vec<u8>> {
    require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"))
}

fn harness() -> SubstrateHarness {
    SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot")
}

/// Send `mail` to the component host and await its reply.
fn host_call<K: Kind + Clone + 'static, R: Kind>(harness: &mut SubstrateHarness, mail: &K) -> R
where
    ComponentHostCapability: HandlesKind<K>,
{
    let host = harness.actor_ref::<ComponentHostCapability>();
    harness
        .execute(vec![("call", HarnessOp::send_and_await_reply(&host, mail))])
        .expect("component host call")
        .reply::<R>("call")
        .expect("decode the host's reply")
}

/// Publish `wasm` with no instance configs, and answer the namespaces it
/// bound, sorted.
fn publish(harness: &mut SubstrateHarness, wasm: &[u8]) -> Vec<String> {
    let published: PublishResult =
        host_call(harness, &Publish { code: Blob::from(wasm.to_vec()), configs: Vec::new() });
    let mut namespaces: Vec<String> = match published {
        PublishResult::Ok { types } => types.into_iter().map(|published| published.namespace).collect(),
        PublishResult::Err { error } => panic!("the publish was refused: {error}"),
    };
    namespaces.sort();
    namespaces
}

fn spawn_gate(harness: &mut SubstrateHarness, key: &str) -> SpawnResult {
    let spawn = Spawn { namespace: GATE.to_owned(), key: Some(key.to_owned()), parent: None, config: Vec::new() };
    host_call(harness, &spawn)
}

fn unpublish(harness: &mut SubstrateHarness, namespace: &str) -> UnpublishResult {
    host_call(harness, &Unpublish { namespace: namespace.to_owned() })
}

/// Poll `ListComponents` until `absent` leaves the live set. The host's
/// unpublish reads the same registry-inventory snapshot this lists, and that
/// snapshot refreshes on its own wake after a drop commits, so an unpublish
/// sent straight after `DropResult::Ok` could still see the dropped route.
fn await_absent(harness: &mut SubstrateHarness, absent: &str) {
    for _ in 0..100 {
        let listed: ListComponentsResult = host_call(harness, &ListComponents {});
        let present = listed.names.iter().any(|name| name == absent);
        if !present {
            return;
        }
    }
    panic!("{absent} never left the live set");
}

#[test]
fn unpublish_of_a_never_published_or_native_namespace_is_refused() {
    // Catches: an `Ok` that pretends a withdrawal happened where nothing was
    // published, or one that withdraws a native publication.
    let mut harness = harness();

    let refused = unpublish(&mut harness, GATE);
    let UnpublishResult::Err { error } = refused else {
        panic!("a never-published namespace is refused: {refused:?}");
    };
    assert!(error.contains(GATE), "the refusal names the namespace: {error}");

    let native = unpublish(&mut harness, "aether.component");
    let UnpublishResult::Err { error } = native else {
        panic!("a native namespace is refused: {native:?}");
    };
    assert!(error.contains("aether.component"), "the refusal names the namespace: {error}");
}

#[test]
fn unpublish_is_refused_while_an_instance_is_live_and_withdraws_after_its_drop() {
    // Catches: a withdrawal under live instances, and an over-broad row
    // removal that takes the module's sibling namespace with it.
    let Some(wasm) = read("republish_group_v1") else {
        return;
    };
    let mut harness = harness();
    assert_eq!(publish(&mut harness, &wasm), [GATE, PEER]);
    let SpawnResult::Spawned { path, .. } = spawn_gate(&mut harness, "a") else {
        panic!("the gate spawns");
    };

    let refused = unpublish(&mut harness, GATE);
    let UnpublishResult::Err { error } = refused else {
        panic!("an unpublish under a live instance is refused: {refused:?}");
    };
    assert!(error.contains(&path.to_string()), "the refusal names the live path: {error}");

    let dropped: DropResult = host_call(&mut harness, &DropComponent { target: path.clone() });
    assert!(matches!(dropped, DropResult::Ok), "the gate drops: {dropped:?}");
    await_absent(&mut harness, &path.to_string());

    let withdrawn = unpublish(&mut harness, GATE);
    let UnpublishResult::Ok { namespace } = withdrawn else {
        panic!("the unpublish commits once its instances are gone: {withdrawn:?}");
    };
    assert_eq!(namespace, GATE);

    let respawned = spawn_gate(&mut harness, "b");
    let SpawnResult::Err { error } = respawned else {
        panic!("a spawn of the withdrawn namespace is refused: {respawned:?}");
    };
    assert!(error.contains("no module publishes"), "the refusal says the namespace is unpublished: {error}");

    let described: DescribeComponentResult = host_call(&mut harness, &DescribeComponent { name: GATE.to_owned() });
    assert!(
        matches!(described, DescribeComponentResult::Err { .. }),
        "the withdrawn namespace describes nothing: {described:?}"
    );

    let sibling = Spawn { namespace: PEER.to_owned(), key: None, parent: None, config: Vec::new() };
    match host_call::<_, SpawnResult>(&mut harness, &sibling) {
        SpawnResult::Spawned { path, .. } => assert_eq!(path.as_str(), PEER),
        other => panic!("the module's sibling namespace still spawns: {other:?}"),
    }
}

#[test]
fn republishing_the_same_bytes_after_a_full_unpublish_binds_them_again() {
    // Catches: a stale publication row or boot entry that keeps the
    // re-publish from binding, or a spawn that finds no module afterwards.
    let Some(wasm) = read("republish_group_v1") else {
        return;
    };
    let mut harness = harness();
    assert_eq!(publish(&mut harness, &wasm), [GATE, PEER]);
    let SpawnResult::Spawned { path, .. } = spawn_gate(&mut harness, "a") else {
        panic!("the gate spawns");
    };
    let dropped: DropResult = host_call(&mut harness, &DropComponent { target: path.clone() });
    assert!(matches!(dropped, DropResult::Ok), "the gate drops: {dropped:?}");
    await_absent(&mut harness, &path.to_string());
    assert!(matches!(unpublish(&mut harness, GATE), UnpublishResult::Ok { .. }));
    assert!(matches!(unpublish(&mut harness, PEER), UnpublishResult::Ok { .. }));

    assert_eq!(publish(&mut harness, &wasm), [GATE, PEER], "the same bytes bind again as a first publish");

    match spawn_gate(&mut harness, "b") {
        SpawnResult::Spawned { path, .. } => assert_eq!(path.as_str(), format!("{GATE}:b")),
        other => panic!("the rebound gate spawns: {other:?}"),
    }
}
