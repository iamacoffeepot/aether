//! Issue 7086: a replace is one group republish (ADR-0241 §7). Every live
//! instance of every namespace the module republishes moves to the
//! successor together, or none does; mail that arrives while a member is
//! prepared reaches the guest that wins, in order; a second republish of the
//! module is refused; and loads and drops of a republishing namespace wait
//! for the answer, then run against the code that won.
//!
//! The fixtures are the #7109 group pair: `test.republish.gate` (instanced)
//! and `test.republish.peer` (a root singleton). v2's gate adds a
//! `GateProbe` row, and v2's peer traps in `on_rehydrate` when its config
//! says so. v3 changes the gate's config kind. `republish_loader` is a guest
//! that loads a component through the host.
//!
//! The tests that hold a republish in flight compose the component host
//! pumped: it dispatches only while the harness drains it, and
//! `step_component_host_through` runs it one envelope at a time.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_actor::ErasedActorRef;
use aether_component::ComponentHostCapability;
use aether_component::component::Prepared;
use aether_data::{ErasedActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{
    DropComponent, DropResult, LoadComponent, LoadResult, ReplaceComponent, ReplaceConfig, ReplaceResult,
};
use aether_substrate::testing::successor_wasm;
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, GateLabelledConfig, GateProbe, GateQuery, GateQueryResult, GuestLoad, PeerConfig,
    TickObserved, WireCountQuery, WireObserved,
};

const GATE: &str = "test.republish.gate";
const PEER: &str = "test.republish.peer";

/// The group pair's two versions and v3, or `None` when they are not built.
struct Group {
    v1: Vec<u8>,
    v2: Vec<u8>,
    v3: Vec<u8>,
}

fn group() -> Option<Group> {
    let read = |stem: &str| require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"));
    Some(Group { v1: read("republish_group_v1")?, v2: read("republish_group_v2")?, v3: read("republish_group_v3")? })
}

fn pooled() -> SubstrateHarness {
    SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot")
}

fn pumped() -> SubstrateHarness {
    SubstrateHarness::builder().with_pumped_component_host().size(64, 48).build().expect("boot")
}

/// Load the gate from `wasm` keyed `key`, with its default config.
fn load_gate(harness: &mut SubstrateHarness, wasm: &[u8], key: &str) -> (ErasedActorRef, ErasedActorPath) {
    let load = LoadComponent {
        wasm: wasm.to_vec(),
        name: Some(key.to_owned()),
        config: Vec::new(),
        export: Some(GATE.to_owned()),
    };
    harness.load_any(&load).unwrap_or_else(|error| panic!("load gate {key}: {error}"))
}

fn load_peer(harness: &mut SubstrateHarness, wasm: &[u8], trap_on_rehydrate: bool) -> ErasedActorRef {
    let load = LoadComponent {
        wasm: wasm.to_vec(),
        name: None,
        config: PeerConfig { trap_on_rehydrate }.encode_into_bytes(),
        export: Some(PEER.to_owned()),
    };
    harness.load_any(&load).unwrap_or_else(|error| panic!("load peer: {error}")).0
}

fn replace(wasm: &[u8]) -> ReplaceComponent {
    ReplaceComponent { wasm: wasm.to_vec(), configs: Vec::new() }
}

/// Send `replace` to the component host and await its answer.
fn republish(harness: &mut SubstrateHarness, replace: &ReplaceComponent) -> ReplaceResult {
    let host = harness.actor_ref::<ComponentHostCapability>();
    harness
        .execute(vec![("replace", HarnessOp::send_and_await_reply(&host, replace))])
        .expect("replace call")
        .reply::<ReplaceResult>("replace")
        .expect("decode ReplaceResult")
}

fn call<K: Kind + Clone + 'static, R: Kind>(harness: &mut SubstrateHarness, to: ErasedActorRef, mail: &K) -> R {
    harness
        .execute(vec![("call", HarnessOp::send_and_await_reply(to, mail))])
        .expect("guest call")
        .reply::<R>("call")
        .expect("decode guest reply")
}

fn expect_ok(result: &ReplaceResult) -> Vec<String> {
    match result {
        ReplaceResult::Ok { types } => types.iter().map(|republished| republished.namespace.clone()).collect(),
        ReplaceResult::Err { error } => panic!("the replace was refused: {error}"),
    }
}

fn expect_err(result: &ReplaceResult) -> &str {
    match result {
        ReplaceResult::Err { error } => error,
        ReplaceResult::Ok { .. } => panic!("the replace was accepted"),
    }
}

#[test]
fn mail_gated_during_prepare_reaches_the_winning_guest_in_order() {
    // Catches: mail that arrives while a member is prepared reaching the old
    // guest (which has no `GateProbe` row and drops it), or released out of
    // arrival order, or the commit sent before every member answered, so a
    // probe lands on neither guest.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = pumped();
    let (gate, _) = load_gate(&mut harness, &fixtures.v1, "a");

    let host = harness.actor_ref::<ComponentHostCapability>();
    let pending = harness.send_deferred(host, &replace(&fixtures.v2));
    harness.step_component_host_through::<Prepared>(1).expect("the gate answers its prepare");

    // The gate answered `Ready` and the host has not run since, so no commit
    // has been sent: every probe waits at the gate.
    for seq in 1..=5 {
        let _ = harness.send_tracked(gate, &GateProbe { seq }).expect("send probe");
    }
    let replaced = harness.await_deferred::<ReplaceResult>(pending).expect("replace reply");

    expect_ok(&replaced);
    let report: GateQueryResult = call(&mut harness, gate, &GateQuery);
    assert_eq!(report.seqs, vec![1, 2, 3, 4, 5], "the winning guest receives every gated probe, in order");
}

#[test]
fn an_abort_after_ready_reinstates_and_rewires_the_ready_member() {
    // Catches: a member whose own prepare succeeded left prepared or unwired
    // when another member refuses, or the refusing member's candidate mail
    // leaving before the group aborted.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = pooled();
    let (gate, _) = load_gate(&mut harness, &fixtures.v1, "a");
    let _peer = load_peer(&mut harness, &fixtures.v1, true);
    let peer_wired = harness.count_observed(WireObserved::NAME);

    let replaced = republish(&mut harness, &replace(&fixtures.v2));

    assert!(expect_err(&replaced).contains("on_rehydrate failed"), "the peer's refusal is reported: {replaced:?}");
    assert!(!harness.accepts(gate, GateProbe::ID), "the ready gate is back on its old guest");
    let gate_wired: CountReport = call(&mut harness, gate, &WireCountQuery);
    assert_eq!(gate_wired.count, 2, "the ready gate's old guest is wired again after its abort");
    assert_eq!(harness.count_observed(WireObserved::NAME), peer_wired + 1, "the refusing peer is wired again too");
    assert_eq!(harness.count_observed(TickObserved::NAME), 0, "the failed candidate's mail never leaves");
}

#[test]
fn every_live_instance_of_every_namespace_commits_together() {
    // Catches: a replace that moves only one instance, or one namespace of
    // the module, leaving the rest on the old code; or a commit that loses a
    // member's rehydrated state.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = pooled();
    let (gate_a, _) = load_gate(&mut harness, &fixtures.v1, "a");
    let (gate_b, _) = load_gate(&mut harness, &fixtures.v1, "b");
    let peer = load_peer(&mut harness, &fixtures.v1, false);
    harness.execute(vec![("bump", HarnessOp::send_and_settle(peer, &Bump))]).expect("bump the peer");

    let replaced = republish(&mut harness, &replace(&fixtures.v2));

    let mut namespaces = expect_ok(&replaced);
    namespaces.sort();
    assert_eq!(namespaces, [GATE, PEER], "the reply names every type the module republished");
    assert!(harness.accepts(gate_a, GateProbe::ID), "gate a runs the successor");
    assert!(harness.accepts(gate_b, GateProbe::ID), "gate b runs the successor");
    let count: CountReport = call(&mut harness, peer, &CountQuery);
    assert_eq!(count.count, 1, "the peer carries its state into the successor");
}

#[test]
fn a_changed_config_kind_needs_a_config_for_each_instance() {
    // Catches: an instance whose type's config kind changed built from its
    // stored config of the old kind, or a refusal that names only the first
    // such instance; and supplied configs not reaching each instance.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = pooled();
    let (gate_a, path_a) = load_gate(&mut harness, &fixtures.v1, "a");
    let (gate_b, path_b) = load_gate(&mut harness, &fixtures.v1, "b");
    let config = |path: &ErasedActorPath, label| ReplaceConfig {
        path: path.clone(),
        config: GateLabelledConfig { label }.encode_into_bytes(),
    };

    let partial = ReplaceComponent { wasm: fixtures.v3.clone(), configs: vec![config(&path_a, 7)] };
    let refused = republish(&mut harness, &partial);

    let error = expect_err(&refused);
    assert!(error.contains(path_b.as_str()), "the refusal names the instance with no config: {error}");
    assert!(!error.contains(&format!("{path_a}:")), "the instance with a config is not refused: {error}");

    let complete =
        ReplaceComponent { wasm: fixtures.v3.clone(), configs: vec![config(&path_a, 7), config(&path_b, 9)] };
    let replaced = republish(&mut harness, &complete);

    expect_ok(&replaced);
    let a: GateQueryResult = call(&mut harness, gate_a, &GateQuery);
    let b: GateQueryResult = call(&mut harness, gate_b, &GateQuery);
    assert_eq!((a.seqs, b.seqs), (vec![7], vec![9]), "each instance is built with its own supplied config");
}

#[test]
fn a_second_republish_of_the_module_is_refused() {
    // Catches: two republishes of one module interleaving their prepares and
    // commits on the same members.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = pumped();
    let (gate, _) = load_gate(&mut harness, &fixtures.v1, "a");

    let host = harness.actor_ref::<ComponentHostCapability>();
    let first = harness.send_deferred(host, &replace(&fixtures.v2));
    harness.step_component_host_through::<Prepared>(1).expect("the gate answers its prepare");
    let successor = successor_wasm(&fixtures.v2, 1);
    let second = harness.send_deferred(host, &replace(&successor));

    let refused = harness.await_deferred::<ReplaceResult>(second).expect("second replace reply");
    let replaced = harness.await_deferred::<ReplaceResult>(first).expect("first replace reply");

    assert!(expect_err(&refused).contains("already republishing"), "the second is refused: {refused:?}");
    expect_ok(&replaced);
    assert!(harness.accepts(gate, GateProbe::ID), "the first republish still commits");
}

#[test]
fn a_load_and_a_drop_mid_republish_run_against_the_winning_code() {
    // Catches: a load of a republishing namespace admitted against the
    // predecessor while members are prepared, spawning an instance the group
    // never moves; or a drop that closes a prepared member under the commit.
    let Some(fixtures) = group() else {
        return;
    };
    let mut harness = pumped();
    let (gate_a, _) = load_gate(&mut harness, &fixtures.v1, "a");
    let (_, path_b) = load_gate(&mut harness, &fixtures.v1, "b");

    let host = harness.actor_ref::<ComponentHostCapability>();
    let replacing = harness.send_deferred(host, &replace(&fixtures.v2));
    harness.step_component_host_through::<Prepared>(2).expect("both gates answer their prepares");
    let dropping = harness.send_deferred(host, &DropComponent { target: path_b });
    let late_load = LoadComponent {
        wasm: fixtures.v1.clone(),
        name: Some("c".to_owned()),
        config: Vec::new(),
        export: Some(GATE.to_owned()),
    };
    let loading = harness.send_deferred(host, &late_load);

    let replaced = harness.await_deferred::<ReplaceResult>(replacing).expect("replace reply");
    let dropped = harness.await_deferred::<DropResult>(dropping).expect("drop reply");
    let loaded = harness.await_deferred::<LoadResult>(loading).expect("load reply");

    expect_ok(&replaced);
    assert!(matches!(dropped, DropResult::Ok), "the drop runs once the replace answers: {dropped:?}");
    match loaded {
        LoadResult::Err { error } => {
            assert!(error.contains("gate_probe"), "the old code is refused against the successor: {error}");
        }
        LoadResult::Ok { path, .. } => panic!("the old code loaded beside the successor at {path}"),
    }
    assert!(harness.accepts(gate_a, GateProbe::ID), "the surviving gate runs the successor");
}

#[test]
fn a_guest_load_of_a_republishing_namespace_waits() {
    // Catches: a load a guest mails to the host treated apart from a host
    // load, so it is admitted against the predecessor mid-republish and
    // spawns the old code beside the successor.
    let Some(fixtures) = group() else {
        return;
    };
    let Some(loader_path) = require_wasm("republish_loader") else {
        return;
    };
    let mut harness = pumped();
    let (gate, _) = load_gate(&mut harness, &fixtures.v1, "a");
    let loader_wasm = fs::read(loader_path).expect("read loader wasm");
    let (loader, _) = harness
        .load_any(&LoadComponent { wasm: loader_wasm, name: None, config: Vec::new(), export: None })
        .expect("load the loader");

    // Race-free by ordering on the pumped host. Once the host has dispatched
    // the gate's `Prepared`, the republish holds the namespace and has only
    // staged its publish: no commit is sent until the host dispatches that
    // publish's completion. The test then waits, dispatching nothing, until
    // the loader's `LoadComponent` is queued for the host, so everything
    // queued ahead of it arrived before any commit went out, and steps the
    // host through it. The commits' `Committed` and `Settled` can only queue
    // behind the load, so the load reaches the host while the republish is
    // still open. A load that skipped the hold would be admitted against the
    // old publication and succeed.
    let host = harness.actor_ref::<ComponentHostCapability>();
    let replacing = harness.send_deferred(host, &replace(&fixtures.v2));
    harness.step_component_host_through::<Prepared>(1).expect("the gate answers its prepare");

    let guest_load = GuestLoad { wasm: fixtures.v1.clone(), name: Some("c".to_owned()), export: Some(GATE.to_owned()) };
    let loading = harness.send_deferred_to(loader, &guest_load).expect("send the guest load");
    harness.await_component_host_queued::<LoadComponent>().expect("the loader's load reaches the host");
    harness.step_component_host_through::<LoadComponent>(1).expect("the host takes the load mid-republish");

    let replaced = harness.await_deferred::<ReplaceResult>(replacing).expect("replace reply");
    let guest_loaded = harness.await_deferred::<LoadResult>(loading).expect("guest load reply");

    expect_ok(&replaced);
    match guest_loaded {
        LoadResult::Err { error } => {
            assert!(error.contains("gate_probe"), "the old code is refused against the successor: {error}");
        }
        LoadResult::Ok { path, .. } => panic!("the old code loaded beside the successor at {path}"),
    }
    assert!(harness.accepts(gate, GateProbe::ID), "the gate runs the successor");
    let names = harness.list_components().expect("list components");
    assert!(
        !names.iter().any(|name| name.ends_with(":c")),
        "the old code never loaded beside the successor: {names:?}"
    );
}
