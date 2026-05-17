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
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate) live in `aether_substrate_bundle::test_bench::test_helpers`
//! (issues 460 + 821).

use aether_data::Kind;
use aether_kinds::{Key, LoadComponent, LoadResult, keycode};
use aether_substrate_bundle::test_bench::{
    TestBench,
    test_helpers::require_runtime,
    visual::{decode_png, differs_from_background},
};

// Force linkage of this crate's own rlib so its `inventory::submit!`
// `KindDescriptor` entries reach `aether_kinds::descriptors::all()`
// in the test binary. The integration test compiles as a separate
// crate that depends on the parent's rlib; without an explicit
// reference the linker strips the descriptor symbols. Same fix as
// PR 432 / PR 434 used for the trunk-rlib pattern.
#[allow(unused_imports)]
use aether_demo_sokoban as _;

/// User-facing component name passed to `LoadComponent`.
const COMPONENT_NAME: &str = "world";

/// Full mailbox address the substrate registers for the loaded
/// component (issue 634 Phase 4 PR 1). Mail to the bare
/// `COMPONENT_NAME` warn-drops as unknown — agents address the
/// trampoline by its full `aether.component.trampoline:NAME` form,
/// which is what `LoadResult.name` returns. Built from
/// `WasmTrampoline::NAMESPACE` — the cap-owned single source of truth
/// post issue 654.
fn component_address() -> String {
    use aether_actor::Actor;
    format!(
        "{}:{}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
        COMPONENT_NAME,
    )
}

/// Load this crate's pre-built wasm into the bench and await
/// `LoadResult`. Panics on load failure so the calling test surfaces
/// the error message rather than wedging on a missing subscription.
fn load_sokoban(bench: &mut TestBench, wasm_path: &std::path::Path) {
    let wasm = std::fs::read(wasm_path).expect("read sokoban wasm");
    let result: LoadResult = bench
        .send_and_await_reply(
            "aether.component",
            &LoadComponent {
                wasm,
                name: Some(COMPONENT_NAME.to_owned()),
            },
        )
        .expect("await load_component reply");
    match result {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

fn assert_draw_triangle_observed(bench: &TestBench) {
    let observed = bench.count_observed("aether.draw_triangle");
    assert!(
        observed >= 1,
        "expected ≥1 aether.draw_triangle observed; got {observed}; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Default-level rendering smoke test. Loads the wasm, advances a
/// few ticks so the cap's accumulator + the render cap's
/// `last_submitted` cache populate, captures, and asserts both
/// that `DrawTriangle` mail flowed and that the captured frame
/// contains pixels diverging from the chassis clear color.
///
/// Pre-iamacoffeepot/aether#847 this required an explicit
/// `nudge_tick` after the advance: `capture` runs `record_frame`
/// with `dispatch_tick=false`, the live `frame_vertices` was
/// drained by the last `advance` render, and capture saw an empty
/// buffer. iamacoffeepot/aether#847 made `record_frame` swap-not-
/// replace into `last_submitted`, so capture replays the last
/// rendered geometry and no nudge is needed.
#[test]
fn default_level_renders_grid_and_player() {
    let Some(wasm_path) = require_runtime("aether_demo_sokoban") else {
        return;
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_sokoban(&mut bench, &wasm_path);

    bench.advance(3).expect("advance");

    let png = bench.capture().expect("capture");
    let img = decode_png(&png).expect("decode capture png");
    assert_draw_triangle_observed(&bench);
    differs_from_background(&img, 5).expect("default level should render visible geometry");
}

/// Key-press path doesn't break rendering. Sokoban's `on_key` handler
/// steps the player one cell on `KEY_D`; the next tick must still
/// produce a renderable frame. Validates that the input dispatch
/// doesn't trap or wedge the component's render loop.
#[test]
fn key_press_keeps_render_path_alive() {
    let Some(wasm_path) = require_runtime("aether_demo_sokoban") else {
        return;
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_sokoban(&mut bench, &wasm_path);

    bench.advance(2).expect("pre-key advance");

    // Press D — sokoban steps the player east; the WASD/arrow mapping
    // lives in `step_delta` inside the demo's lib.rs. `send_bytes`
    // is synchronous-on-settle (iamacoffeepot/aether#834), so the
    // post-key advance below picks up the new player position
    // structurally.
    let key = Key {
        code: keycode::KEY_D,
    };
    bench
        .send_bytes(&component_address(), Key::ID, key.encode_into_bytes())
        .expect("send key");

    bench.advance(2).expect("post-key advance");

    let png = bench.capture().expect("capture");
    let img = decode_png(&png).expect("decode capture png");
    assert_draw_triangle_observed(&bench);
    differs_from_background(&img, 5).expect("frame should still render after key press");
}
