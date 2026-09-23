//! Declared-dependency load refusal (issue #6277).
//!
//! The scenarios use only `HarnessOp` plus ordinary `LoadComponent` /
//! `ReplaceComponent` values. `DependentProbe` declares
//! `depends(ParentPeerTarget)`: loading it alone is a `LoadResult::Err`
//! naming the target, and loading the target first makes the same load
//! `Ok`. Replacing toward the dependent while the target is absent is a
//! `ReplaceResult::Err` that keeps the running module.

use std::fs;

use aether_actor::{Addressable, EMBEDDED_SCOPE};
use aether_component::ComponentHostCapability;
use aether_data::ActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{ExecutionError, HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::Bump;

const PROBE_EXPORT: &str = "test.probe";
const TARGET_EXPORT: &str = "test.parent_peer.target";
const DEPENDENT_EXPORT: &str = "test.parent_peer.dependent";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";

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

fn load_named(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    label: &str,
    parent: Option<&str>,
    name: Option<&str>,
    export: &str,
) -> String {
    match load_result(harness, wasm, label, parent, name, export) {
        LoadResult::Ok { path, .. } => path.to_string(),
        LoadResult::Err { error } => panic!("{label} must load: {error}"),
    }
}

fn fixture_harness() -> Option<(SubstrateHarness, Vec<u8>)> {
    let wasm_path = require_wasm("aether_test_fixtures_bundle")?;
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm))
}

#[test]
fn missing_declared_dependency_refuses_the_load() {
    let Some((mut harness, wasm)) = fixture_harness() else {
        return;
    };

    let outer = load_named(&mut harness, &wasm, "outer", None, Some("outer"), PROBE_EXPORT);

    let refused = load_result(&mut harness, &wasm, "dependent-alone", Some(&outer), None, DEPENDENT_EXPORT);
    let LoadResult::Err { error } = refused else {
        panic!("a load whose declared dependency is not live must be refused");
    };
    assert_eq!(
        error,
        format!("{DEPENDENT_EXPORT} depends on {TARGET_EXPORT}, which is not live"),
        "the refusal names the actor and the missing namespace",
    );

    // Refusal happens before creation: the dependent's address is unknown.
    let unserved = format!("{outer}/{EMBEDDED_SCOPE}:{DEPENDENT_EXPORT}");
    let Err(err) = harness.execute(vec![("void", HarnessOp::send_and_settle(&unserved, &Bump))]) else {
        panic!("mail to the refused address must find no mailbox")
    };
    assert!(
        matches!(
            err,
            ExecutionError::OpFailed { error: SubstrateHarnessError::UnknownMailbox(ref addr), .. }
            if addr == &unserved
        ),
        "the refused load must not have created anything: {err:?}",
    );
    let baseline = harness.count_observed(TICK_OBSERVED);

    load_named(&mut harness, &wasm, "target", Some(&outer), None, TARGET_EXPORT);

    let dependent = load_named(&mut harness, &wasm, "dependent", Some(&outer), None, DEPENDENT_EXPORT);

    // The satisfied load really spawns: the probe answers `Bump` with
    // exactly one `TickObserved`.
    harness.execute(vec![("bump", HarnessOp::send_and_settle(&dependent, &Bump))]).expect("bump the dependent");
    assert_eq!(harness.count_observed(TICK_OBSERVED), baseline + 1, "the loaded dependent must answer mail");
}

#[test]
fn replace_with_unmet_dependency_keeps_running_module() {
    let Some((mut harness, wasm)) = fixture_harness() else {
        return;
    };

    let outer = load_named(&mut harness, &wasm, "outer", None, Some("outer"), PROBE_EXPORT);
    let victim = load_named(&mut harness, &wasm, "victim", Some(&outer), Some("victim"), PROBE_EXPORT);
    let victim_path = ActorPath::new(&victim).expect("a loaded component's address is an actor path");

    let replace = |harness: &mut SubstrateHarness, label: &str, export: Option<&str>| {
        let operation = HarnessOp::send_and_await_reply(
            "aether.component",
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
        format!("{victim} depends on {TARGET_EXPORT}, which is not live"),
        "the refusal names the actor and the missing namespace",
    );

    // A refused replacement keeps the running module: the victim still
    // answers ticks at its mailbox.
    let baseline = harness.count_observed(TICK_OBSERVED);
    harness.execute(vec![("tick", HarnessOp::advance(2))]).expect("post-refusal advance");
    assert!(harness.count_observed(TICK_OBSERVED) > baseline, "the victim must still serve after a refused replace");

    load_named(&mut harness, &wasm, "target", Some(&outer), None, TARGET_EXPORT);

    match replace(&mut harness, "replace-satisfied", Some(DEPENDENT_EXPORT)) {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("a replace whose declared dependency is live must succeed: {error}"),
    }

    // A bare replace reuses the hosted type, which the host does not track,
    // so it checks the entry group's dependencies: the entry probe declares
    // none, and the replace proceeds.
    match replace(&mut harness, "replace-bare", None) {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("a bare replace past a dependency-free entry must succeed: {error}"),
    }
}
