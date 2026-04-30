//! Camera-component scenario tests. Each test boots a `TestBench`,
//! loads this crate's own wasm artifact (built separately for
//! `wasm32-unknown-unknown`), drives the component through its
//! `aether.camera.*` mail surface, and asserts mail-flow / render
//! survivability via `aether-scenario`'s `Check` vocabulary.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_camera_component.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.

use std::path::{Path, PathBuf};

use aether_scenario::{Check, Runner, Script, Step};
use aether_substrate_test_bench::TestBench;

// Force linkage of `aether-camera` so its `inventory::submit!`
// `KindDescriptor` entries reach `aether_kinds::descriptors::all()`
// in the test binary. Without this reference the linker strips the
// transitive crate (host builds of `aether-camera-component` don't
// emit the FFI exports that would otherwise pull camera kinds in),
// and `Step::SendMail` for `aether.camera.*` kinds fails with
// "unknown kind".
use aether_camera as _;

/// Probe for any usable wgpu adapter.
fn has_wgpu_adapter() -> bool {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .is_ok()
}

/// Common setup: skip-if-no-adapter / skip-if-no-wasm. Returns the
/// wasm path on success.
///
/// `AETHER_REQUIRE_RUNTIME=1` flips both skip points into a panic so
/// CI catches a forgotten pre-build entry instead of passing a 30ms
/// vacuous test. CI sets this; local devs leave it unset and keep the
/// existing skip behavior.
fn require_runtime() -> Option<PathBuf> {
    let strict = std::env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    if !has_wgpu_adapter() {
        assert!(
            !strict,
            "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
        );
        eprintln!("skipping: no wgpu adapter available");
        return None;
    }
    match camera_component_wasm() {
        Some(path) => Some(path),
        None => {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but aether_camera_component.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing this crate",
            );
            eprintln!(
                "skipping: aether_camera_component.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown -p aether-camera-component`",
            );
            None
        }
    }
}

/// Locate this crate's wasm artifact. Tries `release` then `debug`
/// so either build profile satisfies the test. Returns `None` if
/// neither exists — the caller skips the test. `CARGO_MANIFEST_DIR`
/// is `crates/aether-camera-component`; the workspace target dir
/// is two levels up.
fn camera_component_wasm() -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let path = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join("aether_camera_component.wasm");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

#[test]
fn camera_component_lifecycle() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let script = Script {
        name: "camera component lifecycle".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                // Use a non-default name so `"camera"` (the chassis
                // sink) doesn't collide — see the chassis-sink-name
                // feedback memory.
                name: Some("cam".to_owned()),
            },
            // A few ticks lets the component finish init, run on_tick,
            // and let the renderer cycle.
            Step::Advance { ticks: 5 },
            Step::Capture,
            Step::Assert {
                check: Check::NotAllBlack,
            },
        ],
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(
        report.passed,
        "camera-component lifecycle failed:\n{:#?}",
        report.steps,
    );
    // Sanity: every step ran (no premature short-circuit).
    assert_eq!(report.steps.len(), 4);
}

/// Phase 2 Phase 1-asserts smoke test: the camera component publishes
/// `aether.camera` to the chassis camera sink every tick. This is the
/// load-bearing flow for camera matrices reaching the GPU; if it
/// regresses, every scene goes back to identity-projection until
/// someone notices visually. `Check::MailObserved` queries the
/// bench's chassis-sink observation log for the kind name — see
/// `TestBench::count_observed`.
#[test]
fn camera_default_orbit_publishes_view_proj() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let script = Script {
        name: "camera default orbit publishes view_proj".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("cam".to_owned()),
            },
            // Five ticks: enough for init + a handful of publishes
            // to surface on the camera sink. The component publishes
            // on every tick after init, so any non-zero count proves
            // the path is alive.
            Step::Advance { ticks: 5 },
            Step::Assert {
                check: Check::MailObserved {
                    name: "aether.camera".to_owned(),
                    min_count: 1,
                },
            },
        ],
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(
        report.passed,
        "camera publish scenario failed:\n{:#?}",
        report.steps,
    );
}

/// Destroy the active default camera ("main") and confirm the
/// substrate stays alive — frame still draws the chassis clear, no
/// panic, no fatal_abort. The component pauses publishing (no further
/// `aether.camera` mail) per its docstring; `count_observed` is
/// cumulative since boot so we can't assert "no further publishes"
/// directly with the current Phase 1 vocabulary, but the survivability
/// half is the load-bearing assertion: a destroy of the active camera
/// shouldn't take down the chassis.
#[test]
fn camera_destroy_main_keeps_substrate_alive() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let script = Script {
        name: "camera destroy main keeps substrate alive".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("cam".to_owned()),
            },
            Step::Advance { ticks: 2 },
            // Baseline: default orbit was publishing before destroy.
            Step::Assert {
                check: Check::MailObserved {
                    name: "aether.camera".to_owned(),
                    min_count: 1,
                },
            },
            // Drop the only camera the component was bootstrapped with.
            Step::SendMail {
                recipient: "cam".to_owned(),
                kind: "aether.camera.destroy".to_owned(),
                params: serde_yml::from_str("name: main").expect("destroy params parse"),
            },
            Step::Advance { ticks: 5 },
            // Survivability: the chassis still renders its clear pass
            // after the active camera was removed. If the component
            // panicked or the substrate wedged, capture would fail or
            // the frame would be all-black.
            Step::Capture,
            Step::Assert {
                check: Check::NotAllBlack,
            },
        ],
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(
        report.passed,
        "camera destroy scenario failed:\n{:#?}",
        report.steps,
    );
}
