//! iamacoffeepot/aether#1037: the queryable capability registry,
//! exercised through a real component-load lifecycle on a `SubstrateHarness`.
//!
//! Each test boots a `SubstrateHarness`, loads (and where relevant replaces /
//! drops) a component, and asks the harness whether the loaded actor
//! `accepts` a kind, as the substrate's capability registry answers. The registry
//! is the prerequisite for the DAG validator's dispatchability check
//! (iamacoffeepot/aether#975 Phase 2). The surface is input-side only —
//! handler kinds + fallback presence; there is deliberately no
//! reply-kind resolution.
//!
//! Skipped when the component wasm hasn't been pre-built (the wasm-load
//! tests only — the harness composes no render cap, so there is no wgpu
//! gate). CI builds every discovered component crate and sets
//! `AETHER_REQUIRE_RUNTIME=1` so a missing pre-build is loud.

use std::path::Path;

use aether_actor::ErasedActorRef;
use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind, KindId};
use aether_fs::{FsCapability, Write};
use aether_harness_substrate::test_helpers::{init_save_sandbox, require_wasm, test_namespace_roots};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, Ping, Tick};
use aether_test_fixtures_kinds::{GateProbe, GateQuery, UnsubscribeKeys};
use std::fs;

// Pin the fixture rlib so its descriptor `inventory::submit!` entries
// land in this test binary (mirrors `cost_table.rs`).
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

/// Load the bundle's singleton `test.probe` export.
fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path) -> (ErasedActorRef, ErasedActorPath) {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    harness
        .load_any(&LoadComponent { wasm, name: None, config: Vec::new(), export: Some("test.probe".to_owned()) })
        .unwrap_or_else(|error| panic!("load_component(test.probe): {error}"))
}

/// A freshly-loaded probe's trampoline mailbox accepts the kinds the
/// probe declares `#[handler]`s for (`Tick`, `Key`, `UnsubscribeKeys`) and
/// rejects kinds it doesn't (`Ping`).
#[test]
fn cap_registry_reports_accepted_kinds() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (probe, _) = load_probe(&mut harness, &wasm_path);

    assert!(harness.accepts(probe, Tick::ID), "probe should accept its declared Tick handler");
    assert!(harness.accepts(probe, UnsubscribeKeys::ID), "probe should accept its declared UnsubscribeKeys handler");
    assert!(!harness.accepts(probe, Ping::ID), "probe has no Ping handler and no fallback — must reject Ping");
}

/// The probe is a strict receiver — no `#[fallback]` — so a kind it doesn't
/// handle is rejected rather than swallowed. (The fallback==true arm of the
/// surface is unit-tested in `aether_substrate::mail::capability`.)
#[test]
fn cap_registry_reports_fallback() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (strict, _) = load_probe(&mut harness, &wasm_path);

    assert!(!harness.accepts(strict, Ping::ID), "a strict receiver rejects an undeclared kind");
}

/// Publishing the gate pair's second version republishes its first with
/// its second (ADR-0241 §7), whose `test.republish.gate` keeps v1's
/// `GateQuery` row and adds a `GateProbe` row, so admission's growth rule
/// passes. The registry reflects the post-replace accept-set at the same
/// mailbox id (stable across replace per ADR-0022): `GateProbe` flips
/// rejected→accepted and `GateQuery` stays accepted.
#[test]
fn cap_registry_updates_on_replace() {
    let (Some(v1_path), Some(v2_path)) = (require_wasm("republish_group_v1"), require_wasm("republish_group_v2"))
    else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let gate = LoadComponent {
        wasm: fs::read(&v1_path).expect("read fixture wasm"),
        name: Some("registry".to_owned()),
        config: Vec::new(),
        export: Some("test.republish.gate".to_owned()),
    };
    let (swappable, _) = harness.load_any(&gate).unwrap_or_else(|error| panic!("load_component(gate v1): {error}"));

    // Pre-replace: v1 accepts GateQuery, rejects GateProbe.
    assert!(harness.accepts(swappable, GateQuery::ID));
    assert!(!harness.accepts(swappable, GateProbe::ID));

    if let Err(error) = harness.publish(fs::read(&v2_path).expect("read fixture wasm")) {
        panic!("publish(gate v2): {error}");
    }

    // Post-replace: v2's accept-set wins.
    assert!(harness.accepts(swappable, GateProbe::ID), "v2 accepts its added GateProbe handler after replace");
    // Both versions declare a GateQuery handler, so it survives the swap.
    assert!(harness.accepts(swappable, GateQuery::ID));
}

/// `aether.component.drop` clears the dropped mailbox's caps — the guest
/// is released before the drop replies, so the mailbox accepts nothing.
#[test]
fn cap_registry_clears_on_drop() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (victim, path) = load_probe(&mut harness, &wasm_path);
    assert!(harness.accepts(victim, Tick::ID), "sanity: loaded probe accepts Tick before drop");

    let host = harness.actor_ref::<ComponentHostCapability>();
    let dropped = harness
        .execute(vec![("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: path }))])
        .expect("drop sequence");
    match dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }

    assert!(!harness.accepts(victim, Tick::ID), "dropped component's mailbox must accept nothing");
}

/// The native+wasm unification guard: a native cap (`aether.fs`)
/// populates the same registry at boot, so its mailbox is queryable
/// for the kinds it declares `#[handler]`s for (e.g. `Write`).
#[test]
fn cap_registry_covers_native_cap() {
    // The fs cap rides `namespace_roots` alone — no wasm, no other caps.
    let sandbox = init_save_sandbox("cap-registry-fs");
    let harness =
        SubstrateHarness::builder().size(64, 48).namespace_roots(test_namespace_roots(sandbox)).build().expect("boot");

    let fs = harness.actor_ref::<FsCapability>().erase();
    assert!(harness.accepts(fs, Write::ID), "the native aether.fs cap should accept its declared Write handler");
    // A native cap with no `#[fallback]` rejects undeclared kinds — a
    // fallback would accept this one, so the refusal also proves there is none.
    assert!(
        !harness.accepts(fs, KindId(0xDEAD_BEEF)),
        "aether.fs is a strict receiver — undeclared kinds are rejected",
    );
}
