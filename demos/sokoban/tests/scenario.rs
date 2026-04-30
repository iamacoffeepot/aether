//! Sokoban demo scenario tests. Each test boots a `TestBench`, loads
//! this crate's own wasm artifact (built separately for
//! `wasm32-unknown-unknown`), and asserts the grid-and-player render
//! path.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_demo_sokoban.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.

use std::path::{Path, PathBuf};

use aether_kinds::keycode;
use aether_scenario::{Check, Runner, Script, Step};
use aether_substrate_test_bench::TestBench;

// Force linkage of this crate's own rlib so its `inventory::submit!`
// `KindDescriptor` entries reach `aether_kinds::descriptors::all()`
// in the test binary. The integration test compiles as a separate
// crate that depends on the parent's rlib; without an explicit
// reference the linker strips the descriptor symbols, and
// `Step::SendMail` for `demo.sokoban.*` kinds fails with "unknown
// kind". Same fix as PR 432 / PR 434 used for the trunk-rlib
// pattern.
use aether_demo_sokoban as _;

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
/// so either build profile satisfies the test. `CARGO_MANIFEST_DIR`
/// is `demos/sokoban`; the workspace target dir is two levels up.
fn sokoban_wasm() -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let path = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join("aether_demo_sokoban.wasm");
        if path.exists() {
            return Some(path);
        }
    }
    None
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
    match sokoban_wasm() {
        Some(path) => Some(path),
        None => {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but aether_demo_sokoban.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing this crate",
            );
            eprintln!(
                "skipping: aether_demo_sokoban.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown -p aether-demo-sokoban`",
            );
            None
        }
    }
}

/// Build a `SendMail` step that fires a direct `aether.tick` to
/// `mailbox` so the next `Capture` frame sees fresh render-sink
/// emissions.
///
/// Background: `TestBench::capture` runs its frame with
/// `dispatch_tick=false` (capture is a state snapshot, not a tick
/// advance). The render sink's vert buffer is consumed-and-replaced
/// every frame, so a component that emits geometry only on
/// `on_tick` paints nothing during the capture frame even though
/// the previous `Advance` ticked it. Pushing `aether.tick` to the
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

/// Default-level rendering smoke test. Loads the wasm, advances a
/// few ticks so init + tick can flush, fires one more tick before
/// capture (so render-sink verts populate), and asserts both that
/// `DrawTriangle` mail flows and that the captured frame contains
/// pixels diverging from the chassis clear color.
#[test]
fn default_level_renders_grid_and_player() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let script = Script {
        name: "sokoban default level renders".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("world".to_owned()),
            },
            Step::Advance { ticks: 3 },
            tick_to("world"),
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
        "sokoban default level scenario failed:\n{:#?}",
        report.steps,
    );
}

/// Key-press path doesn't break rendering. Sokoban's `on_key`
/// handler steps the player one cell on `KEY_D`; the next tick must
/// still produce a renderable frame. Validates that the input
/// dispatch doesn't trap or wedge the component's render loop.
#[test]
fn key_press_keeps_render_path_alive() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let key_d_yaml = format!("code: {}", keycode::KEY_D);
    let script = Script {
        name: "sokoban key press keeps rendering".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("world".to_owned()),
            },
            Step::Advance { ticks: 2 },
            // Press D — sokoban steps the player east; the WASD/arrow
            // mapping lives in `step_delta` inside the demo's lib.rs.
            Step::SendMail {
                recipient: "world".to_owned(),
                kind: "aether.key".to_owned(),
                params: serde_yml::from_str(&key_d_yaml).expect("key params parse"),
            },
            Step::Advance { ticks: 2 },
            tick_to("world"),
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
        "sokoban key-press scenario failed:\n{:#?}",
        report.steps,
    );
}
