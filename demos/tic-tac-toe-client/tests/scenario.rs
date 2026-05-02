//! Tic-tac-toe client demo scenario tests. Each test boots a
//! `TestBench`, loads this crate's own wasm artifact (and, for the
//! click-flow scenario, the server's too), and exercises the
//! per-tick render path plus the input → `PlayMove` → broadcast
//! pipeline.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_demo_tic_tac_toe_client.wasm`
//!   (and the server's binary for the click flow) and skip with an
//!   `eprintln!` when they're absent. CI builds both wasm artifacts
//!   before invoking `cargo test`.

use std::path::{Path, PathBuf};

use aether_scenario::{Check, Runner, Script, Step};
use aether_substrate_bundle::test_bench::TestBench;

// Force linkage of this crate's own rlib so its `inventory::submit!`
// `KindDescriptor` entries reach `aether_kinds::descriptors::all()`
// in the test binary. Without this reference the linker strips the
// transitive crate's descriptor symbols. The crate also re-exports
// the server's kinds (`PlayMove`, `GameState`, `MoveResult`) via its
// dep on `aether-demo-tic-tac-toe`, but the server's inventory items
// only get pulled in when something in the test binary references the
// server crate too — which `Step::SendMail` doesn't (it goes through
// the descriptor list at runtime). The explicit `as _;` on both
// covers both vocabularies.
use aether_demo_tic_tac_toe as _;
use aether_demo_tic_tac_toe_client as _;

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

/// Locate a workspace-target wasm artifact. Tries `release` then
/// `debug` so either build profile satisfies the test.
fn locate_wasm(name: &str) -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let path = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn client_wasm() -> Option<PathBuf> {
    locate_wasm("aether_demo_tic_tac_toe_client.wasm")
}

fn server_wasm() -> Option<PathBuf> {
    locate_wasm("aether_demo_tic_tac_toe_server.wasm")
}

/// Common setup for tests that need only the client wasm. Same
/// shape as the camera / mesh-viewer / sokoban / ttt-server helpers.
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
    match client_wasm() {
        Some(path) => Some(path),
        None => {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but aether_demo_tic_tac_toe_client.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing this crate",
            );
            eprintln!(
                "skipping: aether_demo_tic_tac_toe_client.wasm not built; run \
                 `cargo build --target wasm32-unknown-unknown -p aether-demo-tic-tac-toe-client`",
            );
            None
        }
    }
}

/// Same as `require_runtime` but also requires the server wasm —
/// the click-flow test needs both components co-loaded so the
/// client's `PlayMove` send has somewhere to go and the server's
/// broadcast lands on the loopback.
fn require_runtime_with_server() -> Option<(PathBuf, PathBuf)> {
    let client = require_runtime()?;
    let strict = std::env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    match server_wasm() {
        Some(server) => Some((client, server)),
        None => {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but aether_demo_tic_tac_toe_server.wasm not pre-built",
            );
            eprintln!(
                "skipping: aether_demo_tic_tac_toe_server.wasm not built; run \
                 `cargo build --target wasm32-unknown-unknown -p aether-demo-tic-tac-toe-server`",
            );
            None
        }
    }
}

/// Build a `SendMail` step that fires `aether.tick` at `mailbox` so
/// the next `Capture` frame sees fresh render-sink emissions.
/// Documented workaround for `TestBench::capture` running its frame
/// with `dispatch_tick=false`.
fn tick_to(mailbox: &str) -> Step {
    Step::SendMail {
        recipient: mailbox.to_owned(),
        kind: "aether.tick".to_owned(),
        params: serde_yml::Value::Null,
    }
}

/// Default-render smoke test. The client renders a 3×3 grid of
/// dark cell quads on every tick (no occupancy yet — the server
/// hasn't sent any state). Validates `DrawTriangle` flows + the
/// captured frame contains pixels diverging from the chassis clear
/// color.
#[test]
fn default_render_paints_grid() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let script = Script {
        name: "ttt-client default render paints grid".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("tic_tac_toe.client".to_owned()),
            },
            // First tick: SDK auto-subscribes inputs, on_tick renders.
            // Two more ticks settle the renderer.
            Step::Advance { ticks: 3 },
            tick_to("tic_tac_toe.client"),
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
        "ttt-client default-render scenario failed:\n{:#?}",
        report.steps,
    );
}

/// Click-driven move flow. Loads the server alongside the client so
/// the client's `PlayMove` send (after a synthesised mouse_button
/// over the center cell) has a real recipient. Validates the full
/// chain: `WindowSize` cached → `MouseMove` cached → `MouseButton`
/// hit-tests + sends `PlayMove` → server accepts + broadcasts
/// `tic_tac_toe.game_state` → loopback records the broadcast.
///
/// The mouse coordinates target the center of a 64×48 window
/// (32, 24), which maps to clip-space `(0, 0)` and lands inside cell
/// `(1, 1)` — see `hit_test` in `demos/tic-tac-toe-client/src/lib.rs`.
#[test]
fn click_center_cell_drives_server_broadcast() {
    let Some((client_path, server_path)) = require_runtime_with_server() else {
        return;
    };

    let script = Script {
        name: "ttt-client click drives server broadcast".to_owned(),
        steps: vec![
            // Server first so the SERVER mailbox name (`tic_tac_toe`)
            // is registered before the client tries to resolve a sink
            // pointed at it. Order isn't strictly load-bearing — the
            // sink resolves lazily at send time — but it keeps the
            // load order matching the agent-workflow docstring.
            Step::LoadComponent {
                path: server_path.to_string_lossy().into_owned(),
                name: Some("tic_tac_toe".to_owned()),
            },
            Step::LoadComponent {
                path: client_path.to_string_lossy().into_owned(),
                name: Some("tic_tac_toe.client".to_owned()),
            },
            Step::Advance { ticks: 1 },
            // Cache window size + cursor in the client. The
            // `on_mouse_button` handler bails early if either is None.
            Step::SendMail {
                recipient: "tic_tac_toe.client".to_owned(),
                kind: "aether.window_size".to_owned(),
                params: serde_yml::from_str("width: 64\nheight: 48")
                    .expect("window_size params parse"),
            },
            Step::SendMail {
                recipient: "tic_tac_toe.client".to_owned(),
                kind: "aether.mouse_move".to_owned(),
                params: serde_yml::from_str("x: 32.0\ny: 24.0").expect("mouse_move params parse"),
            },
            // The click. Hit-test → cell (1,1) → PlayMove(1,1) sent
            // to the server. Server accepts, broadcasts game_state.
            Step::SendMail {
                recipient: "tic_tac_toe.client".to_owned(),
                kind: "aether.mouse_button".to_owned(),
                params: serde_yml::Value::Null,
            },
            // One more advance so the queue drain processes the chain
            // (window_size → mouse_move → mouse_button → PlayMove →
            // game_state broadcast → loopback observe).
            Step::Advance { ticks: 1 },
            Step::Assert {
                check: Check::MailObserved {
                    name: "tic_tac_toe.game_state".to_owned(),
                    min_count: 1,
                },
            },
        ],
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(
        report.passed,
        "ttt-client click-flow scenario failed:\n{:#?}",
        report.steps,
    );
}
