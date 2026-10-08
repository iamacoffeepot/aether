//! ADR-0247 rule 3: a guest whose `wire` hook fails fails its birth, and the
//! load that asked for it is told, driven through real `LoadComponent` sends
//! to the component host.
//!
//! The `WireFault` fixture mails the harness observer a marker from `wire`
//! and then returns `Ok`, returns an error, or traps, as its config says. A
//! birth holds the mail its `wire` sent until it goes live, and the observer
//! is an inline route, so a marker a failed birth let out would be recorded
//! before the load's answer arrives.

use std::fs;

use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{SubstrateHarness, SubstrateHarnessError};
use aether_kinds::LoadComponent;
use aether_test_fixtures_kinds::{HookOutcome, WIRE_REFUSAL, WireFaultConfig, WireMarker};

/// The fixture's published name: a root singleton, so every load of it names
/// the same instance.
const WIRE_FAULT: &str = "test.wire_fault";

fn load(wasm: &[u8], outcome: HookOutcome) -> LoadComponent {
    LoadComponent {
        wasm: wasm.to_vec(),
        name: None,
        config: WireFaultConfig { outcome }.encode_into_bytes(),
        export: Some(WIRE_FAULT.to_owned()),
    }
}

fn fixture() -> Option<(SubstrateHarness, Vec<u8>)> {
    let wasm = fs::read(require_wasm("aether_test_fixtures_bundle")?).expect("read fixture wasm");
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm))
}

/// Load the fixture with a failing `outcome`, require the load to be refused,
/// and return the refusal. Then load it again at the same name with a `wire`
/// that succeeds, which must stand up a fresh instance.
///
/// Together the two loads catch: the fault logged and the load answered `Ok`
/// (the first load would succeed); the failed birth's held `wire` mail
/// released (the observer would hold a marker after the first load); a
/// `Starting` route or a tombstone left at the name (the second load would be
/// refused as in use or as retired); and the failed instance left live at
/// the name (the second load would answer with it, and no second `wire`
/// would send the one marker counted at the end).
fn fails_then_loads_fresh(outcome: HookOutcome) -> Option<String> {
    let (mut harness, wasm) = fixture()?;

    let refused = harness.load_any(&load(&wasm, outcome));
    let Err(SubstrateHarnessError::Load(error)) = refused else {
        panic!("a load whose guest fails wire must be refused; got {refused:?}");
    };
    assert_eq!(harness.count_observed(WireMarker::NAME), 0, "a failed birth's wire mail never leaves");

    let (_, path) = harness
        .load_any(&load(&wasm, HookOutcome::Succeeds))
        .unwrap_or_else(|error| panic!("the name is free after a failed birth: {error}"));
    assert_eq!(path.to_string(), WIRE_FAULT);
    assert_eq!(harness.count_observed(WireMarker::NAME), 1, "the second load wired a fresh instance");

    Some(error)
}

#[test]
fn a_guest_that_returns_an_error_from_wire_fails_its_load_with_the_message() {
    let Some(error) = fails_then_loads_fresh(HookOutcome::Refuses) else {
        return;
    };

    assert!(error.contains(WIRE_REFUSAL), "the refusal carries the guest's own message: {error}");
}

#[test]
fn a_guest_that_traps_in_wire_fails_its_load() {
    let Some(error) = fails_then_loads_fresh(HookOutcome::Traps) else {
        return;
    };

    assert!(error.contains("wire trapped"), "the refusal says the guest trapped in wire: {error}");
}
