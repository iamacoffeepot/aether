//! ADR-0241 §5: a load names no key for a singleton, and an unnamed load of
//! an instanced type takes a spawn counter, driven through real
//! `LoadComponent` sends to the component host.
//!
//! A singleton is named by its namespace, so a load that names any key for
//! one, even its own namespace, is refused before the module publishes or its
//! route is staged. An instanced type is named by its load's key, or, with
//! none, by a counter the spawn allocates, so repeated unnamed loads of it
//! never collide.

use std::fs;

use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{SubstrateHarness, SubstrateHarnessError};
use aether_kinds::LoadComponent;

/// The bundle's default export, a singleton.
const SINGLETON_EXPORT: &str = "test.probe";
/// An instanced root export of the same bundle.
const INSTANCED_EXPORT: &str = "test.ui.panel";
const REFUSAL: &str = "is a singleton; a load names no key";

fn load(wasm: &[u8], name: Option<&str>, export: &str) -> LoadComponent {
    LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    }
}

fn fixture() -> Option<(SubstrateHarness, Vec<u8>)> {
    let wasm = fs::read(require_wasm("aether_test_fixtures_bundle")?).expect("read fixture wasm");
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm))
}

/// Catches a name used as a singleton's key (either load would succeed), a
/// namespace-shaped name let through as a second spelling of the unnamed
/// load, and a refusal that runs after the publish or the staging (the
/// following unnamed load would then collide with what the refusal left).
#[test]
fn a_singleton_load_that_names_a_key_is_refused_and_an_unnamed_one_loads() {
    let Some((mut harness, wasm)) = fixture() else {
        return;
    };

    for name in ["cam2", SINGLETON_EXPORT] {
        let refused = harness.load_any(&load(&wasm, Some(name), SINGLETON_EXPORT));
        let Err(SubstrateHarnessError::Load(error)) = refused else {
            panic!("a singleton load named {name:?} must be refused; got {refused:?}");
        };
        assert!(error.contains(SINGLETON_EXPORT), "the refusal names the singleton: {error}");
        assert!(error.contains(REFUSAL), "the refusal states the rule: {error}");
    }

    let (_, path) = harness
        .load_any(&load(&wasm, None, SINGLETON_EXPORT))
        .unwrap_or_else(|error| panic!("an unnamed singleton load must succeed: {error}"));
    assert_eq!(path.to_string(), format!("aether.component/aether.embedded:{SINGLETON_EXPORT}"));
}

/// Catches an unnamed instanced load keyed by its namespace instead of a
/// counter: the second load would collide with the first (`SubnameInUse`).
#[test]
fn unnamed_instanced_loads_take_distinct_counter_keys() {
    let Some((mut harness, wasm)) = fixture() else {
        return;
    };

    let (first, first_path) = harness
        .load_any(&load(&wasm, None, INSTANCED_EXPORT))
        .unwrap_or_else(|error| panic!("the first unnamed instanced load must succeed: {error}"));
    let (second, second_path) = harness
        .load_any(&load(&wasm, None, INSTANCED_EXPORT))
        .unwrap_or_else(|error| panic!("the second unnamed instanced load must succeed: {error}"));

    assert_ne!(first_path, second_path, "each unnamed instanced load takes its own key");
    assert!(harness.published_contract(first).is_some(), "the first instance is live at {first_path}");
    assert!(harness.published_contract(second).is_some(), "the second instance is live at {second_path}");
}
