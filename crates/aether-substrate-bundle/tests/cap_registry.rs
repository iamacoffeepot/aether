//! iamacoffeepot/aether#1037: the queryable capability registry,
//! exercised through a real component-load lifecycle on a `TestBench`.
//!
//! Each test boots a `TestBench`, loads (and where relevant replaces /
//! drops) a component, and asks the substrate's `CapabilityRegistry`
//! whether a mailbox `accepts(kind)` and `has_fallback`. The registry
//! is the prerequisite for the DAG validator's dispatchability check
//! (iamacoffeepot/aether#975 Phase 2). The surface is input-side only —
//! handler kinds + fallback presence; there is deliberately no
//! reply-kind resolution.
//!
//! Skipped when no wgpu adapter is available or the component wasm
//! hasn't been pre-built (same gates as the other bench integration
//! tests). CI builds every discovered component crate and sets
//! `AETHER_REQUIRE_RUNTIME=1` so a missing pre-build is loud.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

use std::path::Path;

use aether_actor::Actor;
use aether_camera::CameraCreate;
use aether_capabilities::{ComponentHostCapability, FsCapability};
use aether_data::{Kind, KindId, MailboxId, mailbox_id_from_name};
use aether_kinds::{
    DropComponent, DropResult, LoadComponent, LoadResult, Ping, ReplaceComponent, ReplaceResult,
    Tick, Write,
};
use aether_substrate_bundle::test_bench::{
    BenchOp, TestBench,
    test_helpers::{has_wgpu_adapter, init_save_sandbox, require_runtime, test_namespace_roots},
};
use aether_test_fixtures::SetRender;
use std::env;
use std::fs;

// Pin the fixture rlib so its descriptor `inventory::submit!` entries
// land in this test binary (mirrors `test_bench_scenario.rs`).
#[allow(unused_imports)]
use aether_test_fixtures as _;

fn load_named(bench: &mut TestBench, wasm_path: &Path, name: &str) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some(name.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component({name}): {error}"),
    }
}

/// wgpu-only skip gate, mirroring `test_bench_scenario.rs`. fs-cap
/// tests need wgpu (the bench builds a `Gpu` at boot) but not a
/// component wasm.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(
        !strict,
        "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
    );
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// A freshly-loaded probe's trampoline mailbox accepts the kinds the
/// probe declares `#[handler]`s for (`Tick`, `SetRender`) and rejects
/// kinds it doesn't (`Ping`).
#[test]
fn cap_registry_reports_accepted_kinds() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_named(&mut bench, &wasm_path, "probe");
    let caps = bench.capability_registry();

    assert!(
        caps.accepts(mbox, Tick::ID),
        "probe should accept its declared Tick handler",
    );
    assert!(
        caps.accepts(mbox, SetRender::ID),
        "probe should accept its declared SetRender handler",
    );
    assert!(
        !caps.accepts(mbox, Ping::ID),
        "probe has no Ping handler and no fallback — must reject Ping",
    );
}

/// The probe is a strict receiver — no `#[fallback]`. Its trampoline
/// mailbox reports `has_fallback == false`, and a kind it doesn't
/// handle is rejected. (The fallback==true arm of the surface is unit-
/// tested in `aether_substrate::mail::capability`.)
#[test]
fn cap_registry_reports_fallback() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_named(&mut bench, &wasm_path, "strict");
    let caps = bench.capability_registry();

    assert!(
        !caps.has_fallback(mbox),
        "probe is a strict receiver; has_fallback must be false",
    );
    // No fallback ⇒ unknown kinds are rejected, not swallowed.
    assert!(!caps.accepts(mbox, Ping::ID));
}

/// `aether.component.replace` swaps the probe wasm for the camera wasm
/// (a distinct handler set). The registry reflects the post-replace
/// accept-set at the same mailbox id (stable across replace per
/// ADR-0022): `SetRender` flips accepted→rejected, `CameraCreate` flips
/// rejected→accepted.
#[test]
fn cap_registry_updates_on_replace() {
    let Some(probe_path) = require_runtime("probe") else {
        return;
    };
    let Some(camera_path) = require_runtime("aether_camera") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_named(&mut bench, &probe_path, "swappable");

    // Pre-replace: probe accepts SetRender, rejects CameraCreate.
    {
        let caps = bench.capability_registry();
        assert!(caps.accepts(mbox, SetRender::ID));
        assert!(!caps.accepts(mbox, CameraCreate::ID));
    }

    let camera_wasm = fs::read(&camera_path).expect("read camera wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &ReplaceComponent {
                    mailbox_id: mbox,
                    wasm: camera_wasm,
                    drain_timeout_ms: None,
                },
            ),
        )])
        .expect("replace sequence");
    match swapped
        .reply::<ReplaceResult>("swap")
        .expect("decode ReplaceResult")
    {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    // Post-replace: the camera's accept-set wins.
    let caps = bench.capability_registry();
    assert!(
        caps.accepts(mbox, CameraCreate::ID),
        "camera should accept its declared CameraCreate handler after replace",
    );
    assert!(
        !caps.accepts(mbox, SetRender::ID),
        "the probe's SetRender handler must be gone after replacing with the camera",
    );
    // Both components declare a Tick handler, so it survives the swap.
    assert!(caps.accepts(mbox, Tick::ID));
}

/// `aether.component.drop` clears the dropped mailbox's caps — once
/// the wasm is unloaded the mailbox accepts nothing.
#[test]
fn cap_registry_clears_on_drop() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_named(&mut bench, &wasm_path, "victim");
    assert!(
        bench.capability_registry().accepts(mbox, Tick::ID),
        "sanity: loaded probe accepts Tick before drop",
    );

    let dropped = bench
        .execute(vec![(
            "drop",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &DropComponent { mailbox_id: mbox },
            ),
        )])
        .expect("drop sequence");
    match dropped
        .reply::<DropResult>("drop")
        .expect("decode DropResult")
    {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }

    let caps = bench.capability_registry();
    assert!(
        !caps.accepts(mbox, Tick::ID),
        "dropped component's mailbox must accept nothing",
    );
    assert!(!caps.has_fallback(mbox));
}

/// The native+wasm unification guard: a native cap (`aether.fs`)
/// populates the same registry at boot, so its mailbox is queryable
/// for the kinds it declares `#[handler]`s for (e.g. `Write`).
#[test]
fn cap_registry_covers_native_cap() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("cap-registry-fs");
    let bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let fs_mbox = mailbox_id_from_name(FsCapability::NAMESPACE);
    let caps = bench.capability_registry();
    assert!(
        caps.accepts(fs_mbox, Write::ID),
        "the native aether.fs cap should accept its declared Write handler",
    );
    // A native cap with no `#[fallback]` rejects undeclared kinds.
    assert!(
        !caps.accepts(fs_mbox, KindId(0xDEAD_BEEF)),
        "aether.fs is a strict receiver — undeclared kinds are rejected",
    );
    assert!(!caps.has_fallback(fs_mbox));
}
