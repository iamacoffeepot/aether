//! Declared-dependency load refusal (issue #6277).
//!
//! The scenario uses only `HarnessOp` plus ordinary `LoadComponent` values.
//! `DependentProbe` declares `depends(ParentPeerTarget)`: loading it alone is
//! a `LoadResult::Err` naming the target, and loading the target first makes
//! the same load `Ok`.

use std::fs;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};

const PROBE_EXPORT: &str = "test.probe";
const TARGET_EXPORT: &str = "test.parent_peer.target";
const DEPENDENT_EXPORT: &str = "test.parent_peer.dependent";

fn load_result(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    label: &str,
    parent: Option<&str>,
    name: Option<&str>,
    export: &str,
) -> LoadResult {
    let component = LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    };
    let operation = match parent {
        Some(parent) => HarnessOp::load_component_under(parent, component),
        None => HarnessOp::send_and_await_reply(ComponentHostCapability::NAMESPACE, &component),
    };
    let result = harness.execute(vec![(label, operation)]).expect("component load operation");
    result.reply::<LoadResult>(label).expect("decode LoadResult")
}

#[test]
fn missing_declared_dependency_refuses_the_load() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let LoadResult::Ok { name: outer, .. } =
        load_result(&mut harness, &wasm, "outer", None, Some("outer"), PROBE_EXPORT)
    else {
        panic!("the dependency-free parent scope must load");
    };

    let refused = load_result(&mut harness, &wasm, "dependent-alone", Some(&outer), None, DEPENDENT_EXPORT);
    let LoadResult::Err { error } = refused else {
        panic!("a load whose declared dependency is not live must be refused");
    };
    assert_eq!(
        error,
        format!("{DEPENDENT_EXPORT} depends on {TARGET_EXPORT}, which is not live"),
        "the refusal names the actor and the missing namespace",
    );

    let LoadResult::Ok { .. } = load_result(&mut harness, &wasm, "target", Some(&outer), None, TARGET_EXPORT) else {
        panic!("the dependency target itself must load");
    };

    match load_result(&mut harness, &wasm, "dependent", Some(&outer), None, DEPENDENT_EXPORT) {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("a load whose declared dependency is live must succeed: {error}"),
    }
}
