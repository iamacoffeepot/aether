//! iamacoffeepot/aether#1037: the queryable capability registry,
//! exercised through a real component-load lifecycle on a `SubstrateHarness`.
//!
//! Each test boots a `SubstrateHarness`, loads (and where relevant replaces /
//! drops) a component, and asks the substrate's `CapabilityRegistry`
//! whether the loaded actor `accepts_actor(kind)`. The registry
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
use aether_data::{ActorPath, Kind, KindId};
use aether_fs::{FsCapability, Write};
use aether_harness_substrate::test_helpers::{init_save_sandbox, require_wasm, test_namespace_roots};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, Ping, ReplaceComponent, ReplaceResult, Tick};
use aether_kit_commons::camera::CameraCreate;
use aether_test_fixtures_kinds::SetRender;
use std::fs;

// Pin the fixture rlib so its descriptor `inventory::submit!` entries
// land in this test binary (mirrors `cost_table.rs`).
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

fn load_named(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str) -> (ErasedActorRef, ActorPath) {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    harness
        .load_any(&LoadComponent { wasm, name: Some(name.to_owned()), config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load_component({name}): {error}"))
}

/// A freshly-loaded probe's trampoline mailbox accepts the kinds the
/// probe declares `#[handler]`s for (`Tick`, `Key`, `SetRender`) and
/// rejects kinds it doesn't (`Ping`).
#[test]
fn cap_registry_reports_accepted_kinds() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (probe, _) = load_named(&mut harness, &wasm_path, "probe");
    let caps = harness.capability_registry();

    assert!(caps.accepts_actor(probe, Tick::ID), "probe should accept its declared Tick handler");
    assert!(caps.accepts_actor(probe, SetRender::ID), "probe should accept its declared SetRender handler");
    assert!(!caps.accepts_actor(probe, Ping::ID), "probe has no Ping handler and no fallback — must reject Ping");
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
    let (strict, _) = load_named(&mut harness, &wasm_path, "strict");
    let caps = harness.capability_registry();

    assert!(!caps.accepts_actor(strict, Ping::ID), "a strict receiver rejects an undeclared kind");
}

/// `aether.component.replace` swaps the probe wasm for `aether-kit-commons`'s
/// non-entry `camera` export (a distinct handler set), exercising
/// `ReplaceComponent.export` (#2027) — the trampoline's hosted type is
/// `probe`, so reaching the camera handler set requires naming the
/// export. The registry reflects the post-replace accept-set at the
/// same mailbox id (stable across replace per ADR-0022): `SetRender`
/// flips accepted→rejected, `CameraCreate` flips rejected→accepted.
#[test]
fn cap_registry_updates_on_replace() {
    let Some(probe_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let Some(kit_path) = require_wasm("aether_kit_commons") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (swappable, path) = load_named(&mut harness, &probe_path, "swappable");

    // Pre-replace: probe accepts SetRender, rejects CameraCreate.
    {
        let caps = harness.capability_registry();
        assert!(caps.accepts_actor(swappable, SetRender::ID));
        assert!(!caps.accepts_actor(swappable, CameraCreate::ID));
    }

    let kit_wasm = fs::read(&kit_path).expect("read kit wasm");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let swapped = harness
        .execute(vec![(
            "swap",
            HarnessOp::send_and_await_reply(
                &host,
                &ReplaceComponent {
                    target: path,
                    wasm: kit_wasm,
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    // ADR-0096 / #2027: select the non-entry `aether.camera`
                    // export from the multi-actor kit module; a bare
                    // replace would reuse the trampoline's probe tag.
                    export: Some("aether.kit.camera".to_owned()),
                },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    // Post-replace: the camera's accept-set wins.
    let caps = harness.capability_registry();
    assert!(
        caps.accepts_actor(swappable, CameraCreate::ID),
        "camera should accept its declared CameraCreate handler after replace",
    );
    assert!(
        !caps.accepts_actor(swappable, SetRender::ID),
        "the probe's SetRender handler must be gone after replacing with the camera",
    );
    // Both components declare a Tick handler, so it survives the swap.
    assert!(caps.accepts_actor(swappable, Tick::ID));
}

/// `aether.component.drop` clears the dropped mailbox's caps — once
/// the wasm is unloaded the mailbox accepts nothing.
#[test]
fn cap_registry_clears_on_drop() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (victim, path) = load_named(&mut harness, &wasm_path, "victim");
    assert!(
        harness.capability_registry().accepts_actor(victim, Tick::ID),
        "sanity: loaded probe accepts Tick before drop"
    );

    let host = harness.actor_ref::<ComponentHostCapability>();
    let dropped = harness
        .execute(vec![("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: path }))])
        .expect("drop sequence");
    match dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }

    let caps = harness.capability_registry();
    assert!(!caps.accepts_actor(victim, Tick::ID), "dropped component's mailbox must accept nothing");
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
    let caps = harness.capability_registry();
    assert!(caps.accepts_actor(fs, Write::ID), "the native aether.fs cap should accept its declared Write handler");
    // A native cap with no `#[fallback]` rejects undeclared kinds — a
    // fallback would accept this one, so the refusal also proves there is none.
    assert!(
        !caps.accepts_actor(fs, KindId(0xDEAD_BEEF)),
        "aether.fs is a strict receiver — undeclared kinds are rejected",
    );
}
