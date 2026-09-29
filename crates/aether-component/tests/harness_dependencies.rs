//! Declared-dependency load refusal (issue #6277).
//!
//! The scenarios use only `HarnessOp` plus ordinary `LoadComponent` /
//! `ReplaceComponent` values. `DependentProbe` declares
//! `depends(ParentPeerTarget)`: loading it alone is a `LoadResult::Err`
//! naming the target, and loading the target first makes the same load
//! `Ok`. Every declarable dependency is a root singleton (ADR-0241 §5), so
//! the check reads the same position wherever the dependent is placed.
//! Replacing toward the dependent while the target is absent is a
//! `ReplaceResult::Err` that keeps the running module. The replaced victim is
//! a keyed stand-in for the target, so it is not the dependency, and its only
//! row (`Bump`) is one the dependent keeps, so the satisfied replace passes
//! the contract check (ADR-0231 §5).

use std::fs;

use aether_actor::ErasedActorRef;
use aether_component::ComponentHostCapability;
use aether_data::ErasedActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{LoadComponent, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::Bump;

const TARGET_EXPORT: &str = "test.parent_peer.target";
const STAND_IN_EXPORT: &str = "test.parent_peer.stand_in";
const DEPENDENT_EXPORT: &str = "test.parent_peer.dependent";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";

fn load(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    name: Option<&str>,
    export: &str,
) -> Result<(ErasedActorRef, ErasedActorPath), SubstrateHarnessError> {
    harness.load_any(&LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    })
}

fn fixture_harness() -> Option<(SubstrateHarness, Vec<u8>)> {
    let wasm_path = require_wasm("aether_test_fixtures_bundle")?;
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm))
}

/// Catches a check that answers nothing (the dependent would load alone), a
/// refusal that runs after the guest is staged (a route would stand), and a
/// check that folds the dependency anywhere but the root (the satisfied load
/// would still be refused).
#[test]
fn missing_declared_dependency_refuses_the_load() {
    let Some((mut harness, wasm)) = fixture_harness() else {
        return;
    };

    let refused = load(&mut harness, &wasm, None, DEPENDENT_EXPORT);
    let Err(SubstrateHarnessError::Load(error)) = refused else {
        panic!("a load whose declared dependency is not live must be refused; got {refused:?}");
    };
    assert_eq!(
        error,
        format!("{DEPENDENT_EXPORT} depends on {TARGET_EXPORT}, which is not live"),
        "the refusal names the actor and the missing namespace",
    );

    // Refusal happens before creation: no dependent stands.
    let listed = harness.list_components().expect("list components");
    assert!(!listed.iter().any(|name| name == DEPENDENT_EXPORT), "the refused load created nothing: {listed:?}");
    let baseline = harness.count_observed(TICK_OBSERVED);

    load(&mut harness, &wasm, None, TARGET_EXPORT).expect("the target loads");
    let (dependent, _) = load(&mut harness, &wasm, None, DEPENDENT_EXPORT).expect("the satisfied dependent loads");

    // The satisfied load really spawns: the probe answers `Bump` with
    // exactly one `TickObserved`.
    harness.execute(vec![("bump", HarnessOp::send_and_settle(dependent, &Bump))]).expect("bump the dependent");
    assert_eq!(harness.count_observed(TICK_OBSERVED), baseline + 1, "the loaded dependent must answer mail");
}

#[test]
fn replace_with_unmet_dependency_keeps_running_module() {
    let Some((mut harness, wasm)) = fixture_harness() else {
        return;
    };

    let (victim, victim_path) = load(&mut harness, &wasm, Some("victim"), STAND_IN_EXPORT).expect("the victim loads");

    let replace = |harness: &mut SubstrateHarness, label: &str, export: Option<&str>| {
        let operation = HarnessOp::send_and_await_reply(
            &harness.actor_ref::<ComponentHostCapability>(),
            &ReplaceComponent {
                target: victim_path.clone(),
                wasm: wasm.clone(),
                drain_timeout_ms: None,
                config: Vec::new(),
                export: export.map(str::to_owned),
            },
        );
        let result = harness.execute(vec![(label, operation)]).expect("replace sequence");
        result.reply::<ReplaceResult>(label).expect("decode ReplaceResult")
    };

    let ReplaceResult::Err { error } = replace(&mut harness, "replace-absent", Some(DEPENDENT_EXPORT)) else {
        panic!("a replace whose declared dependency is not live must be refused");
    };
    assert_eq!(
        error,
        format!("{victim_path} depends on {TARGET_EXPORT}, which is not live"),
        "the refusal names the actor and the missing namespace",
    );

    // A refused replacement keeps the running module: the victim still
    // answers `Bump` at its mailbox with exactly one `TickObserved`.
    let baseline = harness.count_observed(TICK_OBSERVED);
    harness.execute(vec![("bump", HarnessOp::send_and_settle(victim, &Bump))]).expect("bump the victim");
    assert_eq!(
        harness.count_observed(TICK_OBSERVED),
        baseline + 1,
        "the victim must still serve after a refused replace"
    );

    load(&mut harness, &wasm, None, TARGET_EXPORT).expect("the target loads");

    match replace(&mut harness, "replace-satisfied", Some(DEPENDENT_EXPORT)) {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("a replace whose declared dependency is live must succeed: {error}"),
    }

    // A bare replace checks the hosted type, which the satisfied replace just
    // made the dependent probe: its declared dependencies, the target loaded
    // above and the harness observer, are live, and the replace proceeds.
    match replace(&mut harness, "replace-bare", None) {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => {
            panic!("a bare replace whose hosted type has live dependencies must succeed: {error}")
        }
    }
}
