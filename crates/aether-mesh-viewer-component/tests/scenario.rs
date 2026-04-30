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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aether_scenario::{Check, Runner, Script, Step};
use aether_substrate_test_bench::TestBench;

// Force linkage of `aether-mesh-viewer` so its `inventory::submit!`
// `KindDescriptor` entries reach `aether_kinds::descriptors::all()`
// in the test binary. Without this reference the linker strips the
// transitive crate (host builds of `aether-mesh-viewer-component`
// don't emit the FFI exports that would otherwise pull viewer kinds
// in), and `Step::SendMail` for `aether.mesh.load` fails with
// "unknown kind".
use aether_mesh_viewer as _;

/// Process-wide test sandbox for `save://` reads. Each test seeds its
/// fixtures here under unique filenames so parallel test threads don't
/// step on each other. The env var must be set before any TestBench
/// boot — `NamespaceRoots::from_env` reads it once per chassis boot —
/// so we gate all tests through `init_test_save_dir()` first.
static TEST_SAVE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Create the per-process sandbox (idempotent) and point
/// `AETHER_SAVE_DIR` at it. Returns the resolved sandbox path.
///
/// `set_var` is racy with concurrent `getenv` on POSIX, but
/// `OnceLock` linearizes the set, and every test that boots a
/// TestBench gates through this helper first — so by the time any
/// test thread reads env, the set has completed.
fn init_test_save_dir() -> &'static Path {
    TEST_SAVE_DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("aether-mesh-viewer-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test save dir");
        unsafe { std::env::set_var("AETHER_SAVE_DIR", &dir) };
        dir
    })
}

/// Write `contents` into the sandbox at `name`, returning the
/// `save://` path string the scenario uses (the bare filename — the
/// substrate resolves it relative to the namespace root).
fn write_fixture(name: &str, contents: &[u8]) -> String {
    let dir = init_test_save_dir();
    std::fs::write(dir.join(name), contents).expect("write fixture");
    name.to_owned()
}

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

/// Locate this crate's wasm artifact. Tries `release` then `debug`
/// so either build profile satisfies the test. Returns `None` if
/// neither exists — the caller skips the test. `CARGO_MANIFEST_DIR`
/// is `crates/aether-mesh-viewer-component`; the workspace target
/// dir is two levels up.
fn mesh_viewer_wasm() -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let path = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join("aether_mesh_viewer_component.wasm");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Common setup: skip-if-no-adapter / skip-if-no-wasm. Returns the
/// wasm path on success.
fn require_runtime() -> Option<PathBuf> {
    if !has_wgpu_adapter() {
        eprintln!("skipping: no wgpu adapter available");
        return None;
    }
    let path = mesh_viewer_wasm()?;
    Some(path)
}

const BOX_DSL: &[u8] = b"(box 1 1 1 :color 0)\n";
const QUAD_OBJ: &[u8] = b"\
v -0.5 -0.5 0
v  0.5 -0.5 0
v  0.5  0.5 0
v -0.5  0.5 0
f 1 2 3 4
";
const BAD_DSL: &[u8] = b"(box not-a-number 1 1)\n";

/// Build a `SendMail` step that fires a direct `aether.tick` to
/// `mailbox` so the next `Capture` frame sees fresh render-sink
/// emissions.
///
/// Background: `TestBench::capture` runs its frame with
/// `dispatch_tick=false` (capture is a state snapshot, not a tick
/// advance). The render sink's vert buffer is consumed-and-replaced
/// every frame, so a component that only emits geometry on
/// `on_tick` will paint nothing during the capture frame even though
/// the previous `Advance` ticked it. Pushing a `aether.tick` to the
/// component's mailbox right before `Capture` queues a tick that
/// drains alongside the capture request, populating the buffer
/// before the offscreen render reads it.
fn tick_to(mailbox: &str) -> Step {
    Step::SendMail {
        recipient: mailbox.to_owned(),
        kind: "aether.tick".to_owned(),
        params: serde_yml::Value::Null,
    }
}

/// Smoke test: load a `.dsl` box → triangles flow to the render sink
/// every tick → the captured frame contains pixels that diverge from
/// the chassis clear color. Validates the entire DSL load path: the
/// IO sink read, `aether-mesh`'s parser+mesher, the wireframe outline
/// emission, and the per-tick render-sink replay.
#[test]
fn dsl_box_loads_and_renders() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };
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

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(
        report.passed,
        "dsl box scenario failed:\n{:#?}",
        report.steps,
    );
}

/// `.obj` parser smoke. The OBJ path doesn't go through `aether-mesh`'s
/// parser — it's a built-in fan-triangulation parser inside the
/// component — so this test guards against the OBJ branch silently
/// regressing while the DSL branch keeps working.
#[test]
fn obj_quad_loads_and_renders() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };
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

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(
        report.passed,
        "obj quad scenario failed:\n{:#?}",
        report.steps,
    );
}

/// Parse-failure resilience: a known-bad DSL after a known-good DSL
/// must keep the previous mesh visible — the component's contract
/// is "partial parse / mesh failure leaves the previous mesh
/// intact." Loads a good box, advances until triangles flow, loads
/// the bad DSL, advances again, and verifies the frame still
/// diverges from the clear color.
#[test]
fn parse_failure_keeps_prior_mesh() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };
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

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(
        report.passed,
        "parse-failure scenario failed:\n{:#?}",
        report.steps,
    );
}
