//! Camera scenario tests. Each test boots a `TestBench`, loads this
//! crate's own wasm artifact (built separately for
//! `wasm32-unknown-unknown`), drives the component through its
//! `aether.camera.*` mail surface, and asserts mail-flow / render
//! survivability via direct `TestBench` assertions (post-issue-821:
//! the `aether-scenario` Script/Step vocabulary retired in favour of
//! calling the bench methods directly).
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
//! gate) live in `aether_substrate_bundle::test_bench::test_helpers`
//! (issues 460 + 821).

use aether_camera::CameraDestroy;
use aether_kinds::{LoadComponent, LoadResult};
use aether_substrate_bundle::test_bench::{
    TestBench,
    test_helpers::require_runtime,
    visual::{decode_png, not_all_black},
};

// Force linkage of `aether-camera`'s `inventory::submit!` `KindDescriptor`
// entries into this test binary. Cargo treats integration tests as
// separate crates that link against the test target's host rlib, but
// the linker strips inventory submits for kinds the test code doesn't
// statically reference. Without this anchor, `count_observed` against
// the camera-published kinds (and `send_mail::<CameraDestroy>`) would
// still resolve, but other inventory-collected metadata wouldn't —
// keep the anchor for parity with the other component scenario files.
#[allow(unused_imports)]
use aether_camera as _;

/// Component name passed to `LoadComponent`. The full mailbox address
/// the substrate registers is `aether.component.trampoline:cam`
/// (issue 634 Phase 4 PR 1) — bare `"cam"` is not addressable. Camera
/// tests don't currently send any mail to the loaded trampoline by
/// address, so only the load-time name matters here.
const COMPONENT_NAME: &str = "cam";
const COMPONENT_ADDRESS: &str = "aether.component.trampoline:cam";

/// Load this crate's pre-built wasm into the bench and await
/// `LoadResult`. Panics on load failure so the calling test surfaces
/// the error message rather than wedging on a missing subscription.
fn load_camera(bench: &mut TestBench, wasm_path: &std::path::Path) {
    let wasm = std::fs::read(wasm_path).expect("read camera wasm");
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

#[test]
fn camera_component_lifecycle() {
    let Some(wasm_path) = require_runtime("aether_camera") else {
        return;
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_camera(&mut bench, &wasm_path);

    // A few ticks lets the component finish init, run on_tick, and
    // let the renderer cycle.
    bench.advance(5).expect("advance");

    let png = bench.capture().expect("capture");
    let img = decode_png(&png).expect("decode capture png");
    not_all_black(&img).expect("camera scene should not be all black");
}

/// Phase 2 Phase 1-asserts smoke test: the camera component publishes
/// `aether.camera` to the chassis render mailbox every tick. This is
/// the load-bearing flow for camera matrices reaching the GPU; if it
/// regresses, every scene goes back to identity-projection until
/// someone notices visually. `count_observed` queries the bench's
/// chassis-cap observation log for the kind name.
#[test]
fn camera_default_orbit_publishes_view_proj() {
    let Some(wasm_path) = require_runtime("aether_camera") else {
        return;
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_camera(&mut bench, &wasm_path);

    // Five ticks: enough for init + a handful of publishes to surface
    // on the camera sink. The component publishes on every tick after
    // init, so any non-zero count proves the path is alive.
    bench.advance(5).expect("advance");

    let observed = bench.count_observed("aether.camera");
    assert!(
        observed >= 1,
        "expected ≥1 aether.camera observed; got {observed}; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Destroy the active default camera ("main") and confirm the
/// substrate stays alive — frame still draws the chassis clear, no
/// panic, no fatal_abort. The component pauses publishing (no further
/// `aether.camera` mail) per its docstring; `count_observed` is
/// cumulative since boot so we can't assert "no further publishes"
/// directly with the current vocabulary, but the survivability half
/// is the load-bearing assertion: a destroy of the active camera
/// shouldn't take down the chassis.
#[test]
fn camera_destroy_main_keeps_substrate_alive() {
    let Some(wasm_path) = require_runtime("aether_camera") else {
        return;
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_camera(&mut bench, &wasm_path);

    bench.advance(2).expect("pre-destroy advance");
    // Baseline: default orbit was publishing before destroy.
    let pre_destroy = bench.count_observed("aether.camera");
    assert!(
        pre_destroy >= 1,
        "expected ≥1 aether.camera before destroy; got {pre_destroy}; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Drop the only camera the component was bootstrapped with. Per
    // issue 634 Phase 4, agents address loaded components at the
    // trampoline's full name (`aether.component.trampoline:NAME`).
    bench
        .send_mail(
            COMPONENT_ADDRESS,
            &CameraDestroy {
                name: "main".to_owned(),
            },
        )
        .expect("send camera.destroy");
    bench.advance(5).expect("post-destroy advance");

    // Survivability: the chassis still renders its clear pass after
    // the active camera was removed. If the component panicked or the
    // substrate wedged, capture would fail or the frame would be
    // all-black.
    let png = bench.capture().expect("capture after destroy");
    let img = decode_png(&png).expect("decode capture png");
    not_all_black(&img).expect("frame should not be all black after camera destroy");
}
