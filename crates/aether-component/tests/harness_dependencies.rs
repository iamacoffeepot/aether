//! Declared-dependency load refusal (issue #6277).
//!
//! The scenarios use only `HarnessOp` and the harness's load and publish
//! verbs. `DependentProbe` declares
//! `depends(ParentPeerTarget)`: loading it alone is a `LoadResult::Err`
//! naming the target, and loading the target first makes the same load
//! `Ok`. Every declarable dependency is a root singleton (ADR-0241 §5), so
//! the check reads the same position wherever the dependent is placed.
//! Republishing the subject toward a successor that adds
//! `depends(ClipboardCapability)` while no clipboard is live is a
//! `PublishResult::Err` naming the live instance, and it keeps the running
//! module (ADR-0241 §4); with the clipboard composed it succeeds.

use std::fs;

use aether_actor::ErasedActorRef;
use aether_clipboard::{ClipboardCapability, ClipboardParams};
use aether_data::ErasedActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::LoadComponent;
use aether_test_fixtures_bundle::DependentProbe;
use aether_test_fixtures_kinds::Bump;

const TARGET_EXPORT: &str = "test.parent_peer.target";
const DEPENDENT_EXPORT: &str = "test.parent_peer.dependent";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";
const CLIPBOARD: &str = "aether.clipboard";

/// The republish subject's row this file sends: a silent `Bump`. The
/// subject ships only as a cdylib example, so a test casts its `load_any`
/// reference to this instead of naming a type.
#[aether_actor::protocol]
trait SubjectBump {
    fn bump(mail: Bump);
}

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
    let dependent = harness
        .load::<DependentProbe>(LoadComponent { wasm, name: None, config: Vec::new(), export: None })
        .expect("the satisfied dependent loads");

    // The satisfied load really spawns: the probe answers `Bump` with
    // exactly one `TickObserved`.
    harness.execute(vec![("bump", HarnessOp::send_and_settle(&dependent, &Bump))]).expect("bump the dependent");
    assert_eq!(harness.count_observed(TICK_OBSERVED), baseline + 1, "the loaded dependent must answer mail");
}

/// The `republish_subject_<variant>` module's bytes, or `None` to skip.
fn subject_wasm(variant: &str) -> Option<Vec<u8>> {
    Some(fs::read(require_wasm(&format!("republish_subject_{variant}"))?).expect("read fixture wasm"))
}

/// Catches a republish whose successor adds a dependency that is not live
/// reaching the live instance (it would lose its old code for one that
/// cannot run), a refusal that does not name the instance, and a check that
/// refuses a successor whose added dependency is live.
#[test]
fn replace_with_unmet_dependency_keeps_running_module() {
    let (Some(base), Some(depends)) = (subject_wasm("base"), subject_wasm("depends")) else {
        return;
    };

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let load = LoadComponent { wasm: base, name: None, config: Vec::new(), export: None };
    let (subject, subject_path) = harness.load_any(&load).expect("the subject loads");
    let subject = harness.cast::<SubjectBump>(subject).expect("the subject publishes Bump");

    let Err(SubstrateHarnessError::Publish(error)) = harness.publish(depends.clone()) else {
        panic!("a republish whose added dependency is not live must be refused");
    };
    assert_eq!(
        error,
        format!("{subject_path} depends on {CLIPBOARD}, which is not live"),
        "the refusal names the instance and the missing namespace",
    );

    // A refused republish keeps the running module: the subject still
    // answers `Bump` at its mailbox with exactly one `TickObserved`.
    let baseline = harness.count_observed(TICK_OBSERVED);
    harness.execute(vec![("bump", HarnessOp::send_and_settle(&subject, &Bump))]).expect("bump the subject");
    assert_eq!(
        harness.count_observed(TICK_OBSERVED),
        baseline + 1,
        "the subject must still serve after a refused replace"
    );

    let mut satisfied = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<ClipboardCapability>(ClipboardParams::InMemory)
        .build()
        .expect("boot with clipboard");
    satisfied.load_any(&load).expect("the subject loads beside the clipboard");
    if let Err(error) = satisfied.publish(depends) {
        panic!("a republish whose added dependency is live must succeed: {error}");
    }
}
