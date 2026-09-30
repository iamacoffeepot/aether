//! A guest is named as a native actor is (ADR-0241 §5, §6): a declared
//! dependency is a root singleton a peer reaches by type, and a spawn with a
//! parent places a guest only beneath a parent its type declares, at
//! `parent/NS:key`. A short path's hole beneath a guest parent fills from its
//! published module's lineage, the way one beneath a native parent does.
//! Driven through real `Publish` and `Spawn` sends to the component host and
//! `ResolveAddress` sends to the inventory cap.

use std::fs;

use aether_actor::{Addressable, Root, Singleton};
use aether_data::{ErasedActorPath, Kind, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_inventory::InventoryCapability;
use aether_inventory::kinds::{ResolveAddress, ResolveAddressResult};
use aether_kinds::Spawn;
use aether_test_fixtures_bundle::{
    InlineParent, MatrixChild, MatrixParent, ParentPeerCaller, ParentPeerTarget, Probe, SourceObserver,
};
use aether_test_fixtures_kinds::{Bump, TickObserved};
use aether_test_fixtures_short_path::{Branch, Host, Leaf, Placed, Trunk};

const TARGET_EXPORT: &str = ParentPeerTarget::NAMESPACE;
const PROBE_EXPORT: &str = Probe::NAMESPACE;
const MATRIX_PARENT_EXPORT: &str = MatrixParent::NAMESPACE;
const MATRIX_CHILD_EXPORT: &str = MatrixChild::NAMESPACE;
const INLINE_PARENT_EXPORT: &str = InlineParent::NAMESPACE;
const TRUNK_EXPORT: &str = Trunk::NAMESPACE;
const HOST_EXPORT: &str = Host::NAMESPACE;

/// Spawn the published root singleton `R`, returning its canonical path.
fn spawn<R: Root + Singleton>(harness: &mut SubstrateHarness) -> ErasedActorPath {
    let actor = harness.spawn::<R>().unwrap_or_else(|error| panic!("spawn {}: {error}", R::NAMESPACE));
    harness.actor_path(&actor)
}

/// Spawn a `MatrixChild` keyed `k` beneath `parent` through the erased door,
/// which takes a parent whose type the child does not declare.
fn spawn_matrix_child_under(
    harness: &mut SubstrateHarness,
    parent: ErasedActorPath,
) -> Result<ErasedActorPath, SubstrateHarnessError> {
    let spawn = Spawn {
        namespace: MATRIX_CHILD_EXPORT.to_owned(),
        key: Some("k".to_owned()),
        parent: Some(parent),
        config: Vec::new(),
    };
    harness.spawn_any(&spawn).map(|spawned| spawned.path)
}

fn key() -> LoadName {
    LoadName::new("k").expect("a valid load name")
}

/// A harness that also composes the inventory cap, so a scenario resolves
/// short paths the way an external caller does, with the `stem` module
/// published.
fn fixture_of(stem: &str) -> Option<SubstrateHarness> {
    let wasm = fs::read(require_wasm(stem)?).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<InventoryCapability>(())
        .build()
        .expect("boot");
    harness.publish(wasm).unwrap_or_else(|error| panic!("publish {stem}: {error}"));
    Some(harness)
}

fn fixture() -> Option<SubstrateHarness> {
    fixture_of("aether_test_fixtures_bundle")
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
    let Some(mut harness) = fixture() else {
        return;
    };

    let target = spawn::<ParentPeerTarget>(&mut harness);
    let caller = harness.spawn::<ParentPeerCaller>().expect("spawn the caller");
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
/// spawn beneath a parent the child's type does not declare is refused,
/// leaving no route, while the same spawn beneath its declared parent lands
/// at `parent/NS:key`.
#[test]
fn a_spawn_places_a_guest_only_beneath_a_declared_parent() {
    let Some(mut harness) = fixture() else {
        return;
    };

    spawn::<SourceObserver>(&mut harness);
    let matrix_parent = spawn::<MatrixParent>(&mut harness);
    let probe = spawn::<Probe>(&mut harness);

    let Err(SubstrateHarnessError::Spawn(error)) = spawn_matrix_child_under(&mut harness, probe.clone()) else {
        panic!("a spawn beneath an undeclared parent must be refused");
    };
    assert!(error.contains(MATRIX_CHILD_EXPORT) && error.contains(PROBE_EXPORT), "the refusal names both: {error}");
    let stray = format!("{probe}/{MATRIX_CHILD_EXPORT}:k");
    let listed = harness.list_components().expect("list components");
    assert!(!listed.contains(&stray), "the refused spawn left no route: {listed:?}");

    let path = spawn_matrix_child_under(&mut harness, matrix_parent).expect("a spawn beneath the declared parent");
    assert_eq!(path.to_string(), format!("{MATRIX_PARENT_EXPORT}/{MATRIX_CHILD_EXPORT}:k"));
}

/// Catches an index that never reads a published module, reads it only at
/// construction or one level deep, or misses a private child's lineage:
/// holes beneath the trunk and its private inline branch expand to the
/// canonical routes the spawns registered. The spawn runs in the trunk's
/// `Bump` handler, so the settled bump has committed both aliases.
#[test]
fn holes_beneath_guest_parents_expand_through_private_inline_children() {
    let Some(mut harness) = fixture_of("aether_test_fixtures_short_path") else {
        return;
    };

    let trunk = harness.spawn::<Trunk>().expect("spawn the trunk");
    harness.execute(vec![("bump", HarnessOp::send_and_settle(&trunk, &Bump))]).expect("bump the trunk");

    let branch = format!("{TRUNK_EXPORT}/{}:branch", Branch::NAMESPACE);
    assert_eq!(resolve(&mut harness, &format!("{TRUNK_EXPORT}/:branch")), canonical(&branch));
    assert_eq!(
        resolve(&mut harness, &format!("{TRUNK_EXPORT}/:branch/:leaf")),
        canonical(&format!("{branch}/{}:leaf", Leaf::NAMESPACE)),
    );
}

/// Catches guest edges missing for spawn placement: a hole beneath the guest
/// a spawn placed its child under expands to that child.
#[test]
fn a_hole_beneath_a_guest_parent_expands_to_its_spawned_child() {
    let Some(mut harness) = fixture_of("aether_test_fixtures_short_path") else {
        return;
    };

    let host = harness.spawn::<Host>().expect("spawn the host");
    let placed = harness.spawn_child::<Host, Placed>(&host, &key()).expect("spawn the placed child");
    let path = harness.actor_path(&placed);

    assert_eq!(resolve(&mut harness, &format!("{HOST_EXPORT}/:k")), canonical(&path.to_string()));
}

/// Pins the ADR-0166 liveness tie-break over guest lineage: the bundle's
/// composable `test.inline.stateful_child` may sit beneath every actor its
/// module declares, so a hole beneath `test.inline.parent` names two child
/// types. The one holding the key live wins; a key neither holds is refused
/// naming both.
#[test]
fn a_composable_sibling_defers_to_the_live_child_under_a_guest_parents_hole() {
    let Some(mut harness) = fixture() else {
        return;
    };

    let parent = spawn::<InlineParent>(&mut harness);

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
