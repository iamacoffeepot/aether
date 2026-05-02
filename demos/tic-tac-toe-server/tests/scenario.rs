//! Tic-tac-toe demo scenario tests. Each test boots a `TestBench`,
//! loads this crate's own wasm artifact (built separately for
//! `wasm32-unknown-unknown`), and asserts the move-handling +
//! broadcast mail flow.
//!
//! Note: the server component does no rendering — that lives in the
//! sibling `aether-demo-tic-tac-toe-client`. So unlike the camera /
//! mesh-viewer / sokoban scenarios, these tests skip the visual
//! assertion and lean on `Check::MailObserved` / `MailNotObserved`
//! against the `tic_tac_toe.game_state` broadcast the server emits
//! on every accepted move (and *only* accepted moves — rejection
//! short-circuits before the broadcast).
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`). The bench still needs the GPU even for
//!   render-less tests because the chassis owns the offscreen target.
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_demo_tic_tac_toe_server.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.

use std::path::{Path, PathBuf};

use aether_scenario::{Check, Runner, Script, Step};
use aether_substrate_bundle::test_bench::TestBench;

// Force linkage of this crate's own rlib so its `inventory::submit!`
// `KindDescriptor` entries reach `aether_kinds::descriptors::all()`
// in the test binary. Without this reference the linker strips the
// transitive crate's descriptor symbols, and `Step::SendMail` for
// `tic_tac_toe.*` kinds fails with "unknown kind". Same fix as
// PR 432 / 434 / 436 used for the trunk-rlib pattern.
use aether_demo_tic_tac_toe as _;

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
/// so either build profile satisfies the test.
fn ttt_wasm() -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let path = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join("aether_demo_tic_tac_toe_server.wasm");
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
    match ttt_wasm() {
        Some(path) => Some(path),
        None => {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but aether_demo_tic_tac_toe_server.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing this crate",
            );
            eprintln!(
                "skipping: aether_demo_tic_tac_toe_server.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown -p aether-demo-tic-tac-toe`",
            );
            None
        }
    }
}

/// Legal `tic_tac_toe.play_move` accepts and broadcasts. The server
/// only emits `tic_tac_toe.game_state` on `MOVE_OK`, so observing
/// the kind on the loopback proves the move was accepted *and* the
/// broadcast wiring is healthy.
#[test]
fn legal_move_broadcasts_game_state() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let script = Script {
        name: "ttt legal move broadcasts".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("ttt".to_owned()),
            },
            Step::Advance { ticks: 1 },
            // X plays top-left.
            Step::SendMail {
                recipient: "ttt".to_owned(),
                kind: "tic_tac_toe.play_move".to_owned(),
                params: serde_yml::from_str("row: 0\ncol: 0\n_pad: [0, 0]")
                    .expect("play_move params parse"),
            },
            // Drain the move handler so the broadcast lands on the
            // loopback before the assert reads `count_observed`.
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
        "ttt legal-move scenario failed:\n{:#?}",
        report.steps,
    );
}

/// Out-of-bounds move is rejected — `apply_move` returns
/// `MOVE_OUT_OF_BOUNDS` and the broadcast branch is skipped. Negative
/// test: assert no `tic_tac_toe.game_state` ever lands on the
/// loopback. Cumulative observation since boot, so a fresh bench is
/// load-bearing.
#[test]
fn out_of_bounds_move_does_not_broadcast() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let script = Script {
        name: "ttt out-of-bounds move does not broadcast".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("ttt".to_owned()),
            },
            Step::Advance { ticks: 1 },
            // Row 5 is out of bounds (board is 3×3).
            Step::SendMail {
                recipient: "ttt".to_owned(),
                kind: "tic_tac_toe.play_move".to_owned(),
                params: serde_yml::from_str("row: 5\ncol: 0\n_pad: [0, 0]")
                    .expect("play_move params parse"),
            },
            Step::Advance { ticks: 1 },
            Step::Assert {
                check: Check::MailNotObserved {
                    name: "tic_tac_toe.game_state".to_owned(),
                },
            },
        ],
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(
        report.passed,
        "ttt rejection scenario failed:\n{:#?}",
        report.steps,
    );
}

/// `tic_tac_toe.reset` always succeeds (even from a fresh game) and
/// broadcasts the new state. Confirms the reset path's broadcast
/// wiring independently of the move path.
#[test]
fn reset_broadcasts_game_state() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };

    let script = Script {
        name: "ttt reset broadcasts".to_owned(),
        steps: vec![
            Step::LoadComponent {
                path: wasm_path.to_string_lossy().into_owned(),
                name: Some("ttt".to_owned()),
            },
            Step::Advance { ticks: 1 },
            Step::SendMail {
                recipient: "ttt".to_owned(),
                kind: "tic_tac_toe.reset".to_owned(),
                params: serde_yml::Value::Null,
            },
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
        "ttt reset scenario failed:\n{:#?}",
        report.steps,
    );
}
