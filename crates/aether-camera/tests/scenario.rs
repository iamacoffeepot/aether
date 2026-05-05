//! Camera scenario tests. Each test boots a `TestBench`, loads this
//! crate's own wasm artifact (built separately for
//! `wasm32-unknown-unknown`), drives the component through its
//! `aether.camera.*` mail surface, and asserts mail-flow / render
//! survivability via `aether-scenario`'s `Check` vocabulary.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_camera.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate, `Runner::run` + assert postscript) live in
//! `aether_scenario::test_helpers` (issue 460).

use aether_scenario::test_helpers::{require_runtime, run_or_panic};
use aether_scenario::{Check, Script, Step};
use aether_substrate_bundle::test_bench::TestBench;

// Force linkage of `aether-camera`'s `inventory::submit!` `KindDescriptor`
// entries into this test binary. Cargo treats integration tests as
// separate crates that link against the test target's host rlib, but
// the linker strips inventory submits for kinds the test code doesn't
// statically reference. Without this anchor, `Step::SendMail` for
// `aether.camera.*` kinds fails with "unknown kind".
use aether_camera as _;

#[test]
fn camera_component_lifecycle() {
    let Some(wasm_path) = require_runtime("aether_camera") else {
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
    run_or_panic(&mut bench, &script);
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
    let Some(wasm_path) = require_runtime("aether_camera") else {
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
    run_or_panic(&mut bench, &script);
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
    let Some(wasm_path) = require_runtime("aether_camera") else {
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
    run_or_panic(&mut bench, &script);
}
