//! First per-component scenario (ADR-0067, PR 8). Loads a real
//! wasm component into a `TestBench`, advances a few ticks, captures
//! a frame, and asserts the bench survived. The visual assertion is
//! `not_all_black` only — chassis clears to a non-black color so any
//! capture passes — but the test still validates the load path,
//! wasm runtime boot, mail dispatch, tick fanout, and capture
//! end-to-end. Future scenarios assert tighter visual properties once
//! the IO sink + DSL path lets us write actual geometry.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The camera-component wasm hasn't been built — the test reads
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_camera_component.wasm`
//!   and skips with an `eprintln!` when both paths are absent. CI
//!   builds the wasm via the workspace `build` step before invoking
//!   `cargo test`.

use std::path::{Path, PathBuf};

use aether_scenario::{Check, Runner, Script, Step};
use aether_substrate_test_bench::TestBench;

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

/// Locate the camera component's wasm artifact. Tries `release` then
/// `debug` so either build profile satisfies the test. Returns `None`
/// if neither exists — the caller skips the test.
fn camera_component_wasm() -> Option<PathBuf> {
    // CARGO_MANIFEST_DIR is the scenario crate; the workspace target dir
    // is two levels up.
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
    if !has_wgpu_adapter() {
        eprintln!("skipping: no wgpu adapter available");
        return;
    }
    let Some(wasm_path) = camera_component_wasm() else {
        eprintln!(
            "skipping: aether_camera_component.wasm not built; \
             run `cargo build --target wasm32-unknown-unknown -p aether-camera-component`",
        );
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
