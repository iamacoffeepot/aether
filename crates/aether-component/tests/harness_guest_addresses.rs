//! A guest is named as a native actor is (ADR-0241 §5, §6): a declared
//! dependency is a root singleton a peer reaches by type, and `load_under`
//! places a guest only beneath a parent its type declares, at
//! `parent/NS:key`. A short path's hole beneath a guest parent fills from its
//! published module's lineage, the way one beneath a native parent does.
//! Driven through real `LoadComponent` sends to the component host and
//! `ResolveAddress` sends to the inventory cap.

use std::fs;

use aether_actor::{Addressable, ErasedActorRef};
use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_inventory::InventoryCapability;
use aether_inventory::kinds::{ResolveAddress, ResolveAddressResult};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_bundle::ParentPeerCaller;
use aether_test_fixtures_kinds::{Bump, TickObserved};
use aether_test_fixtures_short_path::{Branch, Host, Leaf, Placed, Trunk};

const CALLER_EXPORT: &str = "test.parent_peer.caller";
const TARGET_EXPORT: &str = "test.parent_peer.target";
const PROBE_EXPORT: &str = "test.probe";
const OBSERVER_EXPORT: &str = "test.source_observer";
const MATRIX_PARENT_EXPORT: &str = "test.matrix.parent";
const MATRIX_CHILD_EXPORT: &str = "test.matrix.child";
const INLINE_PARENT_EXPORT: &str = "test.inline.parent";
const TRUNK_EXPORT: &str = Trunk::NAMESPACE;
const HOST_EXPORT: &str = Host::NAMESPACE;
const PLACED_EXPORT: &str = Placed::NAMESPACE;

fn component(wasm: &[u8], name: Option<&str>, export: &str) -> LoadComponent {
    LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    }
}

fn load(harness: &mut SubstrateHarness, wasm: &[u8], export: &str) -> (ErasedActorRef, ErasedActorPath) {
    harness.load_any(&component(wasm, None, export)).unwrap_or_else(|error| panic!("load {export}: {error}"))
}

fn load_under(harness: &mut SubstrateHarness, parent: &ErasedActorPath, wasm: &[u8], export: &str) -> LoadResult {
    let host = harness.actor_ref::<ComponentHostCapability>();
    let operation = HarnessOp::load_component_under(&host, parent.to_string(), component(wasm, Some("k"), export));
    let result = harness.execute(vec![("load-under", operation)]).expect("component load operation");
    result.reply::<LoadResult>("load-under").expect("decode LoadResult")
}

fn fixture() -> Option<(SubstrateHarness, Vec<u8>)> {
    fixture_of("aether_test_fixtures_bundle")
}

/// A harness that also composes the inventory cap, so a scenario resolves
/// short paths the way an external caller does, and the `stem` module.
fn fixture_of(stem: &str) -> Option<(SubstrateHarness, Vec<u8>)> {
    let wasm = fs::read(require_wasm(stem)?).expect("read fixture wasm");
    let harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<InventoryCapability>(())
        .build()
        .expect("boot");
    Some((harness, wasm))
}

/// Resolve `address` through the inventory cap's `ResolveAddress`.
fn resolve(harness: &mut SubstrateHarness, address: &str) -> ResolveAddressResult {
    let inventory = harness.actor_ref::<InventoryCapability>();
    let request = ResolveAddress { address: address.to_owned() };
    let result = harness
        .execute(vec![("resolve", HarnessOp::send_and_await_reply(&inventory, &request))])
        .expect("resolve operation");
    result.reply::<ResolveAddressResult>("resolve").expect("decode ResolveAddressResult")
}

fn canonical(path: &str) -> ResolveAddressResult {
    ResolveAddressResult::Ok { canonical_path: path.to_owned() }
}

/// Catches peer resolution that still folds beneath a parent: a guest's
/// `ctx.send::<R>` to a declared dependency must reach the loaded root guest
/// at `R`'s published name, or the target never observes the `Bump`.
#[test]
fn a_guest_reaches_its_declared_root_dependency_by_type() {
    let Some((mut harness, wasm)) = fixture() else {
        return;
    };

    let (_, target) = load(&mut harness, &wasm, TARGET_EXPORT);
    let (caller, _) = harness
        .load::<ParentPeerCaller>(LoadComponent { wasm, name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load {CALLER_EXPORT}: {error}"));
    assert_eq!(target.to_string(), TARGET_EXPORT, "a root singleton guest is named by its namespace");

    let baseline = harness.count_observed(TickObserved::NAME);
    harness.execute(vec![("bump", HarnessOp::send_and_settle(&caller, &Bump))]).expect("bump the caller");

    assert_eq!(
        harness.count_observed(TickObserved::NAME) - baseline,
        1,
        "the caller's typed send reaches the target; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// Catches placement read from the host rather than the lineage (#6821): a
/// `load_under` beneath a parent the child's type does not declare is refused
/// before the module publishes, leaving no route, while the same load beneath
/// its declared parent lands at `parent/NS:key`.
#[test]
fn load_under_places_a_guest_only_beneath_a_declared_parent() {
    let Some((mut harness, wasm)) = fixture() else {
        return;
    };

    load(&mut harness, &wasm, OBSERVER_EXPORT);
    let (_, matrix_parent) = load(&mut harness, &wasm, MATRIX_PARENT_EXPORT);
    let (_, probe) = load(&mut harness, &wasm, PROBE_EXPORT);

    let LoadResult::Err { error } = load_under(&mut harness, &probe, &wasm, MATRIX_CHILD_EXPORT) else {
        panic!("a load_under beneath an undeclared parent must be refused");
    };
    assert!(error.contains(MATRIX_CHILD_EXPORT) && error.contains(PROBE_EXPORT), "the refusal names both: {error}");
    let stray = format!("{probe}/{MATRIX_CHILD_EXPORT}:k");
    let listed = harness.list_components().expect("list components");
    assert!(!listed.contains(&stray), "the refused load left no route: {listed:?}");

    let LoadResult::Ok { path, .. } = load_under(&mut harness, &matrix_parent, &wasm, MATRIX_CHILD_EXPORT) else {
        panic!("a load_under beneath the declared parent must succeed");
    };
    assert_eq!(path.to_string(), format!("{MATRIX_PARENT_EXPORT}/{MATRIX_CHILD_EXPORT}:k"));
}

/// Catches an index that never reads a published module, reads it only at
/// construction or one level deep, or misses a private child's lineage:
/// holes beneath the trunk and its private inline branch expand to the
/// canonical routes the spawns registered. The spawn runs in the trunk's
/// `Bump` handler, so the settled bump has committed both aliases.
#[test]
fn holes_beneath_guest_parents_expand_through_private_inline_children() {
    let Some((mut harness, wasm)) = fixture_of("aether_test_fixtures_short_path") else {
        return;
    };

    let (trunk, _) = harness
        .load::<Trunk>(LoadComponent { wasm, name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load {TRUNK_EXPORT}: {error}"));
    harness.execute(vec![("bump", HarnessOp::send_and_settle(&trunk, &Bump))]).expect("bump the trunk");

    let branch = format!("{TRUNK_EXPORT}/{}:branch", Branch::NAMESPACE);
    assert_eq!(resolve(&mut harness, &format!("{TRUNK_EXPORT}/:branch")), canonical(&branch));
    assert_eq!(
        resolve(&mut harness, &format!("{TRUNK_EXPORT}/:branch/:leaf")),
        canonical(&format!("{branch}/{}:leaf", Leaf::NAMESPACE)),
    );
}

/// Catches guest edges missing for load placement: a hole beneath the guest
/// a `load_under` placed its child under expands to that child.
#[test]
fn a_hole_beneath_a_guest_parent_expands_to_its_load_under_child() {
    let Some((mut harness, wasm)) = fixture_of("aether_test_fixtures_short_path") else {
        return;
    };

    let (_, host) = load(&mut harness, &wasm, HOST_EXPORT);
    let LoadResult::Ok { path, .. } = load_under(&mut harness, &host, &wasm, PLACED_EXPORT) else {
        panic!("a load_under beneath the declared parent must succeed");
    };

    assert_eq!(resolve(&mut harness, &format!("{HOST_EXPORT}/:k")), canonical(&path.to_string()));
}

/// Pins the ADR-0166 liveness tie-break over guest lineage: the bundle's
/// composable `test.inline.stateful_child` may sit beneath every actor its
/// module declares, so a hole beneath `test.inline.parent` names two child
/// types. The one holding the key live wins; a key neither holds is refused
/// naming both.
#[test]
fn a_composable_sibling_defers_to_the_live_child_under_a_guest_parents_hole() {
    let Some((mut harness, wasm)) = fixture_of("aether_test_fixtures_bundle") else {
        return;
    };

    let (_, parent) = load(&mut harness, &wasm, INLINE_PARENT_EXPORT);

    assert_eq!(
        resolve(&mut harness, &format!("{INLINE_PARENT_EXPORT}/:widget")),
        canonical(&format!("{parent}/test.inline.child:widget")),
    );

    let ResolveAddressResult::Err { error } = resolve(&mut harness, &format!("{INLINE_PARENT_EXPORT}/:gadget")) else {
        panic!("a hole no candidate type holds live must not expand");
    };
    assert!(
        error.contains("no live actor")
            && error.contains("test.inline.child:gadget")
            && error.contains("test.inline.stateful_child:gadget"),
        "the refusal names both candidates: {error}"
    );
}
