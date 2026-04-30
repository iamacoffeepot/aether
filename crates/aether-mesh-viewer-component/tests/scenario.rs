//! Mesh-viewer scenario tests. Each test boots a `TestBench`, loads
//! this crate's own wasm artifact (built separately for
//! `wasm32-unknown-unknown`), seeds a fixture `.dsl` / `.obj` file
//! into the substrate's `save://` namespace, and drives the component
//! through `aether.mesh.load` to verify the load → parse → render
//! pipeline end-to-end.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_mesh_viewer_component.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate, `save://` sandbox, `tick_to`, `Runner::run` + assert
//! postscript) live in `aether_scenario::test_helpers` (issue 460).
//! Per issue 464, the sandbox is plumbed via
//! `TestBench::builder().namespace_roots(...)` rather than env-var
//! mutation.

use aether_scenario::test_helpers::{
    init_save_sandbox, require_runtime, run_or_panic, test_namespace_roots, tick_to, write_fixture,
};
use aether_scenario::{Check, Script, Step};
use aether_substrate_test_bench::TestBench;

// Force linkage of `aether-mesh-viewer` so its `inventory::submit!`
// `KindDescriptor` entries reach `aether_kinds::descriptors::all()`
// in the test binary. Without this reference the linker strips the
// transitive crate (host builds of `aether-mesh-viewer-component`
// don't emit the FFI exports that would otherwise pull viewer kinds
// in), and `Step::SendMail` for `aether.mesh.load` fails with
// "unknown kind".
use aether_mesh_viewer as _;

const BOX_DSL: &[u8] = b"(box 1 1 1 :color 0)\n";
const QUAD_OBJ: &[u8] = b"\
v -0.5 -0.5 0
v  0.5 -0.5 0
v  0.5  0.5 0
v -0.5  0.5 0
f 1 2 3 4
";
const BAD_DSL: &[u8] = b"(box not-a-number 1 1)\n";

/// Smoke test: load a `.dsl` box → triangles flow to the render sink
/// every tick → the captured frame contains pixels that diverge from
/// the chassis clear color. Validates the entire DSL load path: the
/// IO sink read, `aether-mesh`'s parser+mesher, the wireframe outline
/// emission, and the per-tick render-sink replay.
#[test]
fn dsl_box_loads_and_renders() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer_component") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let path = write_fixture("dsl_box.dsl", BOX_DSL);

    let script = Script {
        name: "mesh viewer loads DSL box".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("mv".to_owned()),
            },
            // First tick triggers the load; the read reply lands on
            // a later tick. A handful of ticks ensures the cache is
            // populated and several render-sink emissions land.
            Step::Advance { ticks: 1 },
            Step::SendMail {
                recipient: "mv".to_owned(),
                kind: "aether.mesh.load".to_owned(),
                params: serde_yml::from_str(&format!("namespace: save\npath: {path}"))
                    .expect("dsl load params parse"),
            },
            Step::Advance { ticks: 5 },
            tick_to("mv"),
            Step::Capture,
            Step::Assert {
                check: Check::MailObserved {
                    name: "aether.draw_triangle".to_owned(),
                    min_count: 1,
                },
            },
            Step::Assert {
                check: Check::DiffersFromBackground { tolerance: 5 },
            },
        ],
    };

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    run_or_panic(&mut bench, &script);
}

/// `.obj` parser smoke. The OBJ path doesn't go through `aether-mesh`'s
/// parser — it's a built-in fan-triangulation parser inside the
/// component — so this test guards against the OBJ branch silently
/// regressing while the DSL branch keeps working.
#[test]
fn obj_quad_loads_and_renders() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer_component") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let path = write_fixture("obj_quad.obj", QUAD_OBJ);

    let script = Script {
        name: "mesh viewer loads OBJ quad".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("mv".to_owned()),
            },
            Step::Advance { ticks: 1 },
            Step::SendMail {
                recipient: "mv".to_owned(),
                kind: "aether.mesh.load".to_owned(),
                params: serde_yml::from_str(&format!("namespace: save\npath: {path}"))
                    .expect("obj load params parse"),
            },
            Step::Advance { ticks: 5 },
            tick_to("mv"),
            Step::Capture,
            Step::Assert {
                check: Check::MailObserved {
                    name: "aether.draw_triangle".to_owned(),
                    min_count: 1,
                },
            },
            Step::Assert {
                check: Check::DiffersFromBackground { tolerance: 5 },
            },
        ],
    };

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    run_or_panic(&mut bench, &script);
}

/// Parse-failure resilience: a known-bad DSL after a known-good DSL
/// must keep the previous mesh visible — the component's contract
/// is "partial parse / mesh failure leaves the previous mesh
/// intact." Loads a good box, advances until triangles flow, loads
/// the bad DSL, advances again, and verifies the frame still
/// diverges from the clear color.
#[test]
fn parse_failure_keeps_prior_mesh() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer_component") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let good = write_fixture("good.dsl", BOX_DSL);
    let bad = write_fixture("bad.dsl", BAD_DSL);

    let script = Script {
        name: "parse failure keeps prior mesh".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("mv".to_owned()),
            },
            Step::Advance { ticks: 1 },
            Step::SendMail {
                recipient: "mv".to_owned(),
                kind: "aether.mesh.load".to_owned(),
                params: serde_yml::from_str(&format!("namespace: save\npath: {good}"))
                    .expect("good load params parse"),
            },
            Step::Advance { ticks: 5 },
            // Baseline: the good mesh is publishing.
            Step::Assert {
                check: Check::MailObserved {
                    name: "aether.draw_triangle".to_owned(),
                    min_count: 1,
                },
            },
            // Now hand the viewer something it can't parse.
            Step::SendMail {
                recipient: "mv".to_owned(),
                kind: "aether.mesh.load".to_owned(),
                params: serde_yml::from_str(&format!("namespace: save\npath: {bad}"))
                    .expect("bad load params parse"),
            },
            Step::Advance { ticks: 5 },
            tick_to("mv"),
            Step::Capture,
            // The cached triangle list should be intact — the
            // captured frame still has non-clear-color geometry.
            Step::Assert {
                check: Check::DiffersFromBackground { tolerance: 5 },
            },
        ],
    };

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    run_or_panic(&mut bench, &script);
}
