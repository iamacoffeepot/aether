//! A guest is named as a native actor is (ADR-0241 §5, §6): a declared
//! dependency is a root singleton a peer reaches by type, and `load_under`
//! places a guest only beneath a parent its type declares, at
//! `parent/NS:key`. Driven through real `LoadComponent` sends to the
//! component host.

use std::fs;

use aether_actor::ErasedActorRef;
use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::{Bump, TickObserved};

const CALLER_EXPORT: &str = "test.parent_peer.caller";
const TARGET_EXPORT: &str = "test.parent_peer.target";
const PROBE_EXPORT: &str = "test.probe";
const OBSERVER_EXPORT: &str = "test.source_observer";
const MATRIX_PARENT_EXPORT: &str = "test.matrix.parent";
const MATRIX_CHILD_EXPORT: &str = "test.matrix.child";

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
    let wasm = fs::read(require_wasm("aether_test_fixtures_bundle")?).expect("read fixture wasm");
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm))
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
    let (caller, _) = load(&mut harness, &wasm, CALLER_EXPORT);
    assert_eq!(target.to_string(), TARGET_EXPORT, "a root singleton guest is named by its namespace");

    let baseline = harness.count_observed(TickObserved::NAME);
    harness.execute(vec![("bump", HarnessOp::send_and_settle(caller, &Bump))]).expect("bump the caller");

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
