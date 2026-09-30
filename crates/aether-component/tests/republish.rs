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
//! that loads a component through the host. The courier pair's v2 mails the
//! host from `on_rehydrate`, so its load or drop arrives on the republish's
//! own commit chain. The keep pair (issue 7125) is a keeper that moves a held
//! reply and its count out of itself in `on_dehydrate`, beside a refuser
//! whose v2 traps in `on_rehydrate`.
//!
//! The tests that hold a republish in flight compose the component host
//! pumped: it dispatches only while the harness drains it, and
//! `step_component_host_through` runs it one envelope at a time.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_actor::{ActorRef, ErasedActorRef};
use aether_component::ComponentHostCapability;
use aether_component::component::Prepared;
use aether_data::{ErasedActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SendTarget, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{DropComponent, DropResult, InstanceConfig, LoadComponent, LoadResult, Publish, PublishResult};
use aether_substrate::testing::successor_wasm;
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, CourierConfig, CourierQuery, CourierQueryResult, GateLabelledConfig, GateProbe,
    GateQuery, GateQueryResult, GuestLoad, HeldRequest, HeldRequestResult, PeerConfig, ReleaseCarried, TickObserved,
    WireCountQuery, WireObserved,
};
use aether_test_fixtures_republish::{Keeper, ProbeGate};

const GATE: &str = "test.republish.gate";
const PEER: &str = "test.republish.peer";
const COURIER: &str = "test.republish.courier";
const PARCEL: &str = "test.republish.parcel";
const KEEPER: &str = "test.republish.keep.keeper";
const REFUSER: &str = "test.republish.keep.refuser";

/// The courier's row this file sends: `CourierQuery -> CourierQueryResult`.
/// Both courier versions ship only as cdylib examples, so the test casts its
/// `load_any` reference to this instead of naming a type.
#[aether_actor::protocol]
trait CourierRow {
    fn query(mail: CourierQuery) -> CourierQueryResult;
}

/// The group gate's row `an_abort_after_ready_reinstates_and_rewires_the_ready_member`
/// sends: `WireCountQuery -> CountReport`. This scenario's gate never leaves
/// v1 (its republish aborts), and v1 ships only as a cdylib example.
#[aether_actor::protocol]
trait GateWired {
    fn wired(mail: WireCountQuery) -> CountReport;
}

/// The group gate's row `a_changed_config_kind_needs_a_config_for_each_instance`
/// sends: `GateQuery -> GateQueryResult`. Both v1 and v3 ship only as cdylib
/// examples.
#[aether_actor::protocol]
trait GateRow {
    fn query(mail: GateQuery) -> GateQueryResult;
}

/// The group peer's rows `every_live_instance_of_every_namespace_commits_together`
/// sends: a silent `Bump` and `CountQuery -> CountReport`. Every peer version
/// ships only as a cdylib example.
#[aether_actor::protocol]
trait GroupPeer {
    fn bump(mail: Bump);
    fn count(mail: CountQuery) -> CountReport;
}

/// The loader's row this file sends: `GuestLoad -> LoadResult`. The loader
/// ships only as a cdylib example, so the test casts its `load_any`
/// reference to this instead of naming a type.
#[aether_actor::protocol]
trait LoaderRow {
    fn load(mail: GuestLoad) -> LoadResult;
}

/// The courier pair's two versions, or `None` when they are not built.
struct Couriers {
    v1: Vec<u8>,
    v2: Vec<u8>,
}

fn couriers() -> Option<Couriers> {
    let read = |stem: &str| require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"));
    Some(Couriers { v1: read("republish_courier_v1")?, v2: read("republish_courier_v2")? })
}

/// Load `export` from `wasm`, keyed `key` when given, with no config.
fn load_export(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    export: &str,
    key: Option<&str>,
) -> (ErasedActorRef, ErasedActorPath) {
    let load = LoadComponent {
        wasm: wasm.to_vec(),
        name: key.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    };
    harness.load_any(&load).unwrap_or_else(|error| panic!("load {export}: {error}"))
}

/// Republish the courier pair's v2, building the courier's successor with
/// `config`, and answer what its mail to the host came back with.
fn republish_courier(
    harness: &mut SubstrateHarness,
    v2: &[u8],
    courier: (ErasedActorRef, ErasedActorPath),
    config: &CourierConfig,
) -> Vec<String> {
    let (courier, path) = courier;
    let configs = vec![InstanceConfig { path, config: config.encode_into_bytes() }];

    expect_ok(&republish(harness, v2, configs));
    let courier = harness.cast::<CourierRow>(courier).expect("the courier publishes CourierQuery");
    call::<_, _, CourierQueryResult>(harness, &courier, &CourierQuery).outcomes
}

/// The keep pair's two versions, or `None` when they are not built.
struct Keeps {
    v1: Vec<u8>,
    v2: Vec<u8>,
}

fn keeps() -> Option<Keeps> {
    let read = |stem: &str| require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"));
    Some(Keeps { v1: read("republish_keep_v1")?, v2: read("republish_keep_v2")? })
}

/// Send the keeper a `ReleaseCarried` and wait for its chain to settle.
fn release_keeper(harness: &mut SubstrateHarness, keeper: ActorRef<Keeper>) {
    harness
        .execute(vec![("release", HarnessOp::send_and_settle(&keeper, &ReleaseCarried))])
        .expect("release the keeper");
}

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

/// A `Publish` of `wasm` with no configs, for a republish a test holds in
/// flight.
fn publish(wasm: &[u8]) -> Publish {
    Publish { code: wasm.to_vec().into(), configs: Vec::new() }
}

/// Republish `wasm` with `configs` through the harness and answer the host's
/// verdict.
fn republish(harness: &mut SubstrateHarness, wasm: &[u8], configs: Vec<InstanceConfig>) -> PublishResult {
    match harness.publish_configured(wasm.to_vec(), configs) {
        Ok(types) => PublishResult::Ok { types },
        Err(SubstrateHarnessError::Publish(error)) => PublishResult::Err { error },
        Err(error) => panic!("the publish must answer: {error}"),
    }
}

fn call<K: Kind + Clone + 'static, I, R: Kind>(
    harness: &mut SubstrateHarness,
    to: impl SendTarget<K, I>,
    mail: &K,
) -> R {
    harness
        .execute(vec![("call", HarnessOp::send_and_await_reply(to, mail))])
        .expect("guest call")
        .reply::<R>("call")
        .expect("decode guest reply")
}

fn expect_ok(result: &PublishResult) -> Vec<String> {
    match result {
        PublishResult::Ok { types } => types.iter().map(|published| published.namespace.clone()).collect(),
        PublishResult::Err { error } => panic!("the republish was refused: {error}"),
    }
}

fn expect_err(result: &PublishResult) -> &str {
    match result {
        PublishResult::Err { error } => error,
        PublishResult::Ok { .. } => panic!("the republish was accepted"),
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
    // Typed by the successor (`ProbeGate`, the shared lib type) rather than
    // v1's own cdylib-only `Gate`: the probes below name `GateProbe`, a row
    // only the winning successor publishes, and the harness's adopt checks
    // only that the route is `Live` and a component trampoline.
    let gate = harness
        .load::<ProbeGate>(LoadComponent {
            wasm: fixtures.v1.clone(),
            name: Some("a".to_owned()),
            config: Vec::new(),
            export: None,
        })
        .unwrap_or_else(|error| panic!("load gate a: {error}"));

    let host = harness.actor_ref::<ComponentHostCapability>();
    let pending = harness.send_deferred(host, &publish(&fixtures.v2));
    harness.step_component_host_through::<Prepared>(1).expect("the gate answers its prepare");

    // The gate answered `Ready` and the host has not run since, so no commit
    // has been sent: every probe waits at the gate.
    for seq in 1..=5 {
        let _ = harness.send_tracked(&gate, &GateProbe { seq }).expect("send probe");
    }
    let replaced = harness.await_deferred::<PublishResult>(pending).expect("replace reply");

    expect_ok(&replaced);
    let report: GateQueryResult = call(&mut harness, &gate, &GateQuery);
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
    let gate_wired_ref = harness.cast::<GateWired>(gate).expect("the gate publishes WireCountQuery");
    let _peer = load_peer(&mut harness, &fixtures.v1, true);
    let peer_wired = harness.count_observed(WireObserved::NAME);

    let replaced = republish(&mut harness, &fixtures.v2, Vec::new());

    assert!(expect_err(&replaced).contains("on_rehydrate failed"), "the peer's refusal is reported: {replaced:?}");
    assert!(!harness.accepts(gate, GateProbe::ID), "the ready gate is back on its old guest");
    let gate_wired: CountReport = call(&mut harness, &gate_wired_ref, &WireCountQuery);
    assert_eq!(gate_wired.count, 2, "the ready gate's old guest is wired again after its abort");
    assert_eq!(harness.count_observed(WireObserved::NAME), peer_wired + 1, "the refusing peer is wired again too");
    assert_eq!(harness.count_observed(TickObserved::NAME), 0, "the failed candidate's mail never leaves");
}

#[test]
fn an_abort_gives_each_ready_member_its_dehydrated_state_back() {
    // Catches: a reinstated Ready member missing the value its
    // `on_dehydrate` moved out, or a moved-out `Held` whose requester is
    // never answered.
    let Some(fixtures) = keeps() else {
        return;
    };
    let mut harness = pooled();
    let keeper = harness
        .load::<Keeper>(LoadComponent { wasm: fixtures.v1.clone(), name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load {KEEPER}: {error}"));
    let _refuser = load_export(&mut harness, &fixtures.v1, REFUSER, None);
    // The keeper's mailbox is FIFO, so the request reaches it ahead of the
    // host's prepare.
    let pending = harness.send_deferred_to(&keeper, &HeldRequest { tag: 7 }).expect("send the held request");

    let replaced = republish(&mut harness, &fixtures.v2, Vec::new());

    assert!(expect_err(&replaced).contains("on_rehydrate failed"), "the refuser's refusal is reported: {replaced:?}");
    let kept: CountReport = call(&mut harness, &keeper, &CountQuery);
    assert_eq!(kept.count, 1, "the reinstated keeper has the count its dehydrate moved out");

    release_keeper(&mut harness, keeper);
    let answered = harness.await_deferred::<HeldRequestResult>(pending).expect("the held reply");
    assert_eq!(answered.tag, 7, "the moved-out held reply answers its requester");
}

#[test]
fn a_held_unsaved_refusal_gives_the_member_its_dehydrated_state_back() {
    // Catches: a member refusing as held-unsaved drops the `Held` its
    // `on_dehydrate` had already moved into encoded state.
    let Some(fixtures) = keeps() else {
        return;
    };
    let mut harness = pooled();
    let keeper = harness
        .load::<Keeper>(LoadComponent { wasm: fixtures.v1.clone(), name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load {KEEPER}: {error}"));
    let first = harness.send_deferred_to(&keeper, &HeldRequest { tag: 7 }).expect("send the first held request");
    // The second request's reply stays live as a stray, so the dehydrate
    // refuses.
    let second = harness.send_deferred_to(&keeper, &HeldRequest { tag: 8 }).expect("send the second held request");

    let replaced = republish(&mut harness, &fixtures.v2, Vec::new());

    assert!(
        expect_err(&replaced).contains("held reply is live and was not saved"),
        "the keeper's held-unsaved refusal is reported: {replaced:?}"
    );
    let kept: CountReport = call(&mut harness, &keeper, &CountQuery);
    assert_eq!(kept.count, 2, "the reinstated keeper has the count its dehydrate moved out");

    release_keeper(&mut harness, keeper);
    let first = harness.await_deferred::<HeldRequestResult>(first).expect("the saved held reply");
    let second = harness.await_deferred::<HeldRequestResult>(second).expect("the stray held reply");
    assert_eq!((first.tag, second.tag), (7, 8), "both held replies answer their requesters");
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
    let peer = harness.cast::<GroupPeer>(peer).expect("the peer publishes Bump and CountQuery");
    harness.execute(vec![("bump", HarnessOp::send_and_settle(&peer, &Bump))]).expect("bump the peer");

    let replaced = republish(&mut harness, &fixtures.v2, Vec::new());

    let mut namespaces = expect_ok(&replaced);
    namespaces.sort();
    assert_eq!(namespaces, [GATE, PEER], "the reply names every type the module republished");
    assert!(harness.accepts(gate_a, GateProbe::ID), "gate a runs the successor");
    assert!(harness.accepts(gate_b, GateProbe::ID), "gate b runs the successor");
    let count: CountReport = call(&mut harness, &peer, &CountQuery);
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
    let gate_a = harness.cast::<GateRow>(gate_a).expect("gate a publishes GateQuery");
    let gate_b = harness.cast::<GateRow>(gate_b).expect("gate b publishes GateQuery");
    let config = |path: &ErasedActorPath, label| InstanceConfig {
        path: path.clone(),
        config: GateLabelledConfig { label }.encode_into_bytes(),
    };

    let refused = republish(&mut harness, &fixtures.v3, vec![config(&path_a, 7)]);

    let error = expect_err(&refused);
    assert!(error.contains(path_b.as_str()), "the refusal names the instance with no config: {error}");
    assert!(!error.contains(&format!("{path_a}:")), "the instance with a config is not refused: {error}");

    let replaced = republish(&mut harness, &fixtures.v3, vec![config(&path_a, 7), config(&path_b, 9)]);

    expect_ok(&replaced);
    let a: GateQueryResult = call(&mut harness, &gate_a, &GateQuery);
    let b: GateQueryResult = call(&mut harness, &gate_b, &GateQuery);
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
    let first = harness.send_deferred(host, &publish(&fixtures.v2));
    harness.step_component_host_through::<Prepared>(1).expect("the gate answers its prepare");
    let successor = successor_wasm(&fixtures.v2, 1);
    let second = harness.send_deferred(host, &publish(&successor));

    let refused = harness.await_deferred::<PublishResult>(second).expect("second replace reply");
    let replaced = harness.await_deferred::<PublishResult>(first).expect("first replace reply");

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
    let replacing = harness.send_deferred(host, &publish(&fixtures.v2));
    harness.step_component_host_through::<Prepared>(2).expect("both gates answer their prepares");
    let dropping = harness.send_deferred(host, &DropComponent { target: path_b });
    let late_load = LoadComponent {
        wasm: fixtures.v1.clone(),
        name: Some("c".to_owned()),
        config: Vec::new(),
        export: Some(GATE.to_owned()),
    };
    let loading = harness.send_deferred(host, &late_load);

    let replaced = harness.await_deferred::<PublishResult>(replacing).expect("replace reply");
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
    let loader = harness.cast::<LoaderRow>(loader).expect("the loader publishes GuestLoad");

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
    let replacing = harness.send_deferred(host, &publish(&fixtures.v2));
    harness.step_component_host_through::<Prepared>(1).expect("the gate answers its prepare");

    let guest_load = GuestLoad { wasm: fixtures.v1.clone(), name: Some("c".to_owned()), export: Some(GATE.to_owned()) };
    let loading = harness.send_deferred_to(&loader, &guest_load).expect("send the guest load");
    harness.await_component_host_queued::<LoadComponent>().expect("the loader's load reaches the host");
    harness.step_component_host_through::<LoadComponent>(1).expect("the host takes the load mid-republish");

    let replaced = harness.await_deferred::<PublishResult>(replacing).expect("replace reply");
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

#[test]
fn a_load_on_the_commit_chain_runs_against_the_successor() {
    // Catches: a load the committing candidate held being parked in its own
    // republish. It rides the commit's chain, the replace answers only once
    // that chain settles, and the parked load's held reply keeps it open, so
    // the replace never answers and this test fails at the settlement cap.
    // Running at once, the load is admitted against the published successor
    // and the courier hears its answer before the replace does.
    let Some(fixtures) = couriers() else {
        return;
    };
    let mut harness = pooled();
    let courier = load_export(&mut harness, &fixtures.v1, COURIER, None);

    let config = CourierConfig { wasm: fixtures.v2.clone(), drop: None };
    let outcomes = republish_courier(&mut harness, &fixtures.v2, courier, &config);

    assert_eq!(outcomes, [format!("load ok {PARCEL}:late")], "the successor's load was served: {outcomes:?}");
}

#[test]
fn a_drop_on_the_commit_chain_closes_the_member_after_it_commits() {
    // Catches: a drop the committing candidate held being parked in its own
    // republish, which deadlocks the replace as the load does; and a drop
    // handed to a member ahead of its commit, or a dropped member's
    // `Committed` wedging the ledger, so the replace never answers. The drop
    // lands behind the parcel's `Commit`, so the parcel commits and then
    // closes, and its name is spent.
    let Some(fixtures) = couriers() else {
        return;
    };
    let mut harness = pooled();
    let courier = load_export(&mut harness, &fixtures.v1, COURIER, None);
    let (_, parcel) = load_export(&mut harness, &fixtures.v1, PARCEL, Some("a"));

    let config = CourierConfig { wasm: Vec::new(), drop: Some(parcel.clone()) };
    let outcomes = republish_courier(&mut harness, &fixtures.v2, courier, &config);

    assert_eq!(outcomes, ["drop ok"], "the successor's drop was served: {outcomes:?}");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let again = harness
        .execute(vec![("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: parcel }))])
        .expect("drop call")
        .reply::<DropResult>("drop")
        .expect("decode DropResult");
    assert!(matches!(again, DropResult::Err { .. }), "the dropped parcel's name is spent: {again:?}");
}
