//! Camera scenario tests. Each test boots a `SubstrateHarness`, loads
//! `aether-kit`'s wasm artifact (built separately for
//! `wasm32-unknown-unknown`) selecting the non-entry `camera` export
//! (ADR-0096), drives the `CameraComponent` through its
//! `aether.kit.camera.*` mail surface, and asserts the projected frame /
//! render survivability via direct `SubstrateHarness` assertions (post-issue-821:
//! the `aether-scenario` Script/Step vocabulary retired in favour of
//! calling the harness methods directly).
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_kit.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate) live in `aether_harness_substrate_capture::test_helpers`
//! (issues 460 + 821).

use aether_actor::ActorRef;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, require_runtime};
use aether_harness_substrate_capture::visual::{background_top_left, coverage, decode_png, not_all_black};
use aether_kinds::LoadComponent;
use aether_kit::camera::{CameraComponent, CameraDestroy};
use aether_math::Rgb;
use aether_render::{DrawTriangle, Vertex};

// Force linkage of `aether-kit`'s `inventory::submit!` `KindDescriptor`
// entries into this test binary. Cargo treats integration tests as
// separate crates that link against the test target's host rlib, but
// the linker strips inventory submits for kinds the test code doesn't
// statically reference. Without this anchor, `send_and_settle::<CameraDestroy>`
// would still resolve, but other inventory-collected metadata wouldn't —
// keep the anchor for parity with the other component scenario files.
#[allow(unused_imports)]
use aether_kit as _;
use std::fs;
use std::path::Path;

/// Load `aether-kit`'s pre-built wasm into the harness, selecting the
/// `camera` export (ADR-0096; the kit is defaultless per ADR-0138, so
/// the export selector is required), and await `LoadResult`. Panics on load failure so
/// the calling test surfaces the error message rather than wedging on
/// a missing subscription.
fn load_camera(harness: &mut SubstrateHarness, wasm_path: &Path) -> ActorRef<CameraComponent> {
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    harness
        .load::<CameraComponent>(LoadComponent {
            wasm,
            name: None,
            config: Vec::new(),
            export: Some("aether.kit.camera".to_owned()),
        })
        .unwrap_or_else(|error| panic!("load_component: {error}"))
}

#[test]
fn camera_component_lifecycle() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };

    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");
    load_camera(&mut harness, &wasm_path);

    // A few ticks lets the component finish init, run on_tick, and
    // let the renderer cycle.
    let result = harness
        .execute(vec![("advance", HarnessOp::advance(5)), ("snap", HarnessOp::capture())])
        .expect("advance + capture");
    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    not_all_black(&img).expect("camera scene should not be all black");
}

/// Capture one frame drawing a world-space triangle centred on the origin —
/// verts `(-0.5, -0.5, 0)`, `(0.5, -0.5, 0)`, `(0, 0.5, 0)` — and return the
/// fraction of the frame it covers against the clear color.
fn capture_triangle_coverage(harness: &mut SubstrateHarness, label: &'static str) -> f32 {
    let color = Rgb { r: 0.9, g: 0.3, b: 0.2 };
    let corner = |x: f32, y: f32| Vertex { x, y, z: 0.0, color };
    let triangle = DrawTriangle { verts: [corner(-0.5, -0.5), corner(0.5, -0.5), corner(0.0, 0.5)] };
    let captured = harness
        .execute(vec![(label, HarnessOp::capture_with_mails(vec![envelope("aether.render", &triangle)], Vec::new()))])
        .expect("capture-with-mails");
    let img = decode_png(captured.captured(label).expect("capture step ran")).expect("decode capture png");
    coverage(&img, background_top_left(&img), 5)
}

/// The default camera (a frozen orbit, `speed: 0.0`) projects world
/// geometry. This is the load-bearing flow for camera matrices reaching the
/// GPU: if it regresses, every scene falls back to the render cap's identity
/// projection until someone notices visually. Under identity the triangle
/// spans NDC area 0.5 of 4, about 12.5% of the frame; through the boot pose
/// (orbit distance 3, pitch 0.3, 60° field of view) it projects to about
/// 0.34–0.46 by 0.55 NDC, about 2–3% of the frame depending on the aspect.
/// A camera whose matrix never reaches the GPU leaves the second capture at
/// the identity footprint.
#[test]
fn camera_default_pose_projects_world_geometry() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };

    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");
    let identity = capture_triangle_coverage(&mut harness, "identity");
    assert!(
        (0.08..0.17).contains(&identity),
        "before any camera loads, the triangle draws under the identity projection (~12.5% of the frame); \
         got {identity:.3}",
    );

    load_camera(&mut harness, &wasm_path);
    // Five ticks: enough for init and a handful of `Render`-stage publishes,
    // each replacing the render cap's projection latest-wins.
    harness.execute(vec![("advance", HarnessOp::advance(5))]).expect("advance");
    let projected = capture_triangle_coverage(&mut harness, "projected");
    assert!(projected > 0.005, "the projected triangle must stay visible through the camera; got {projected:.3}");
    assert!(
        projected < identity * 0.5,
        "the camera's projection must reach the GPU: the triangle still covers {projected:.3} of the frame, near \
         the identity footprint {identity:.3} it falls back to when no view_proj arrives",
    );
}

/// Destroy the active default camera ("main") and confirm the
/// substrate stays alive — frame still draws the chassis clear, no
/// panic, no `fatal_abort`. The component pauses publishing (no further
/// `aether.view_projection` mail) per its docstring. Survivability is the
/// load-bearing assertion here — a destroy of the active camera shouldn't
/// take down the chassis; the projection itself is proven by
/// `camera_default_pose_projects_world_geometry`.
#[test]
fn camera_destroy_main_keeps_substrate_alive() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };

    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");
    let camera = load_camera(&mut harness, &wasm_path);

    harness.execute(vec![("pre", HarnessOp::advance(2))]).expect("pre-destroy advance");

    // Drop the only camera the component was bootstrapped with, then
    // advance and capture.
    //
    // Survivability: the chassis still renders its clear pass after
    // the active camera was removed. If the component panicked or the
    // substrate wedged, capture would fail or the frame would be
    // all-black.
    let result = harness
        .execute(vec![
            ("destroy", HarnessOp::send_and_settle(&camera, &CameraDestroy { name: "main".to_owned() })),
            ("post", HarnessOp::advance(5)),
            ("snap", HarnessOp::capture()),
        ])
        .expect("destroy + advance + capture");
    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    not_all_black(&img).expect("frame should not be all black after camera destroy");
}
