//! An unselected `LoadComponent` (ADR-0241 §9, superseding ADR-0138's
//! default slot): it succeeds only when the module exports exactly one
//! non-boot type, and is refused, naming every export, otherwise.

use std::fs;

use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{SubstrateHarness, SubstrateHarnessError};
use aether_kinds::LoadComponent;

/// One of the bundle fixture's exports, reachable only by naming it.
const BUNDLE_PROBE_EXPORT: &str = "test.probe";
/// The single-actor fixture's own namespace, the only type it exports.
const SOLE_EXPORT_NAMESPACE: &str = "test.stateful.typed";

fn load(wasm: &[u8], export: Option<&str>) -> LoadComponent {
    LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: export.map(str::to_owned) }
}

fn fixture(crate_name: &str) -> Option<(SubstrateHarness, Vec<u8>)> {
    let wasm = fs::read(require_wasm(crate_name)?).expect("read fixture wasm");
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm))
}

/// Catches a regression that still requires an explicit export even when
/// the module carries exactly one loadable type.
#[test]
fn an_unselected_load_of_a_single_export_module_succeeds() {
    let Some((mut harness, wasm)) = fixture("aether_test_fixtures_stateful_reshaped") else {
        return;
    };

    let (_, path) = harness
        .load_any(&load(&wasm, None))
        .unwrap_or_else(|error| panic!("an unselected load of a single-export module must succeed: {error}"));
    assert_eq!(path.to_string(), SOLE_EXPORT_NAMESPACE);
}

/// Catches a regression that picks one export of a module exporting more
/// than one non-boot type: an unselected load must be refused, naming every
/// export, never silently picking one by list position.
#[test]
fn an_unselected_load_of_a_multi_export_module_is_refused() {
    let Some((mut harness, wasm)) = fixture("aether_test_fixtures_bundle") else {
        return;
    };

    let refused = harness.load_any(&load(&wasm, None));
    let Err(SubstrateHarnessError::Load(error)) = refused else {
        panic!("an unselected load of a multi-export module must be refused; got {refused:?}");
    };
    assert!(error.contains(BUNDLE_PROBE_EXPORT), "the refusal names the module's exports: {error}");
    assert!(error.contains("export"), "the refusal names the missing export selector: {error}");
}

/// The same module still loads once its export is named explicitly: the
/// refusal above is about the missing selector, not the module itself.
#[test]
fn a_named_export_of_a_multi_export_module_still_loads() {
    let Some((mut harness, wasm)) = fixture("aether_test_fixtures_bundle") else {
        return;
    };

    let (_, path) = harness
        .load_any(&load(&wasm, Some(BUNDLE_PROBE_EXPORT)))
        .unwrap_or_else(|error| panic!("a named export load must still succeed: {error}"));
    assert_eq!(path.to_string(), BUNDLE_PROBE_EXPORT);
}
