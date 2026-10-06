//! Issue 7154: `Publish` binds a module's namespaces by mail (ADR-0241 §3,
//! §9). Identical bytes change nothing, a first publish binds every exported
//! namespace so a `Spawn` can stand one up, a successor republishes every
//! live instance of its namespaces as one group (§7), and a published
//! namespace no live actor is named by describes from its module.
//!
//! The fixtures are the #7109 group pair: `test.republish.gate` (instanced)
//! and `test.republish.peer` (a root singleton that reports `WireObserved`
//! each time it is wired). v2's gate adds a `GateProbe` row. Skipped when the
//! fixture wasm hasn't been built (`require_wasm`); CI pre-builds it and sets
//! `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a hard panic there.

use std::fs;

use aether_actor::HandlesKind;
use aether_component::ComponentHostCapability;
use aether_data::{Blob, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{
    DescribeComponent, DescribeComponentResult, LoadComponent, Publish, PublishResult, Spawn, SpawnResult,
};
use aether_test_fixtures_kinds::{GateProbe, GateQuery, PeerConfig, WireObserved};

const GATE: &str = "test.republish.gate";
const PEER: &str = "test.republish.peer";

/// The group pair's two versions, or `None` when they are not built.
struct Group {
    v1: Vec<u8>,
    v2: Vec<u8>,
}

fn group() -> Option<Group> {
    let read = |stem: &str| require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"));
    Some(Group { v1: read("republish_group_v1")?, v2: read("republish_group_v2")? })
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

fn load_gate(wasm: &[u8], key: &str) -> LoadComponent {
    LoadComponent { wasm: wasm.to_vec(), name: Some(key.to_owned()), config: Vec::new(), export: Some(GATE.to_owned()) }
}

#[test]
fn identical_bytes_publish_again_with_no_swap() {
    // Catches: a publish of the module that already holds every namespace
    // treated as a successor, which would prepare and commit the live peer
    // and wire its candidate again.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = harness();
    let first = publish(&mut harness, &fixtures.v1);
    let peer = LoadComponent {
        wasm: fixtures.v1.clone(),
        name: None,
        config: PeerConfig { trap_on_rehydrate: false }.encode_into_bytes(),
        export: Some(PEER.to_owned()),
    };
    harness.load_any(&peer).unwrap_or_else(|error| panic!("load the peer: {error}"));
    let wired = harness.count_observed(WireObserved::NAME);

    let again = publish(&mut harness, &fixtures.v1);

    assert_eq!(again, first, "the no-op publish names the same namespaces");
    assert_eq!(harness.count_observed(WireObserved::NAME), wired, "no instance was swapped");
}

#[test]
fn a_first_publish_binds_every_export_for_a_spawn() {
    // Catches: a publish that answers without binding the module, so the
    // spawn finds no module for the namespace; or a reply that omits one of
    // the namespaces it bound.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = harness();

    let namespaces = publish(&mut harness, &fixtures.v1);

    assert_eq!(namespaces, [GATE, PEER], "the reply names every namespace the module exports");
    let spawn =
        Spawn { namespace: GATE.to_owned(), key: Some("a".to_owned()), parent: None, config: Vec::new(), code: None };
    match host_call::<_, SpawnResult>(&mut harness, &spawn) {
        SpawnResult::Spawned { path, .. } => assert_eq!(path.as_str(), format!("{GATE}:a")),
        other => panic!("the published gate spawns: {other:?}"),
    }
}

#[test]
fn a_successor_publish_moves_every_live_instance() {
    // Catches: a publish of a successor that repoints the table while the
    // live gates keep running the predecessor, which has no `GateProbe` row.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = harness();
    let (gate_a, _) = harness.load_any(&load_gate(&fixtures.v1, "a")).unwrap_or_else(|error| panic!("{error}"));
    let (gate_b, _) = harness.load_any(&load_gate(&fixtures.v1, "b")).unwrap_or_else(|error| panic!("{error}"));

    let namespaces = publish(&mut harness, &fixtures.v2);

    assert_eq!(namespaces, [GATE, PEER]);
    assert!(harness.accepts(gate_a, GateProbe::ID), "gate a runs the successor");
    assert!(harness.accepts(gate_b, GateProbe::ID), "gate b runs the successor");
}

#[test]
fn a_published_namespace_with_no_live_actor_describes_from_its_module() {
    // Catches: a describe that reads only live actors, so an instanced type
    // nothing is spawned of cannot be introspected before its first spawn.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = harness();
    publish(&mut harness, &fixtures.v1);

    let described: DescribeComponentResult = host_call(&mut harness, &DescribeComponent { name: GATE.to_owned() });

    let DescribeComponentResult::Ok { capabilities } = described else {
        panic!("the published gate describes: {described:?}");
    };
    assert!(
        capabilities.handlers.iter().any(|handler| handler.id == GateQuery::ID),
        "the published gate's rows are described: {capabilities:?}"
    );
}
