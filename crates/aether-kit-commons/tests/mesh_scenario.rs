//! Mesh-viewer scenario tests. Each test boots a `SubstrateHarness`, loads
//! `aether-kit-commons`'s wasm artifact (built separately for
//! `wasm32-unknown-unknown`) selecting the non-entry `mesh_viewer`
//! export (ADR-0096), seeds a fixture `.dsl` / `.obj` file into the
//! substrate's `save://` namespace, and drives the component through
//! `aether.kit.mesh.load` to verify the load → parse → render pipeline
//! end-to-end.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_kit_commons.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate, `save://` sandbox) live in
//! `aether_harness_substrate_capture::test_helpers` (issues 460 +
//! 821). Per issue 464, the sandbox is plumbed via
//! `SubstrateHarness::builder().namespace_roots(...)` rather than env-var
//! mutation.

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{
    envelope, init_save_sandbox, require_runtime, test_namespace_roots, write_fixture,
};
use aether_harness_substrate_capture::visual::{Image, decode_png, differs_from_background};
use aether_kinds::{LoadComponent, LoadResult, MeshLoadResult, Render, WindowId, WindowSize};
use aether_kit_commons::camera::{CameraOrbitSet, OrbitParams};
use aether_kit_commons::mesh::LoadMesh;
use core::f32::consts::FRAC_PI_2;

// Force linkage of `aether-kit-commons`'s `inventory::submit!` `KindDescriptor`
// entries into this test binary. Cargo treats integration tests as
// separate crates that link against the test target's host rlib, but
// the linker strips inventory submits for kinds the test code doesn't
// statically reference.
#[allow(unused_imports)]
use aether_kit_commons as _;
use std::fs;
use std::path::Path;

/// User-facing component name passed to `LoadComponent`.
const COMPONENT_NAME: &str = "mv";
const CAMERA_COMPONENT_NAME: &str = "aether.kit.camera";
const OUTLINE_WINDOW_WIDTH: u32 = 768;
const OUTLINE_WINDOW_HEIGHT: u32 = 576;
const OUTLINE_WINDOW_ID: WindowId = WindowId(1);

/// Full mailbox address the substrate registers for the loaded
/// component (issue 634 Phase 4 PR 1). Mail to the bare
/// `COMPONENT_NAME` warn-drops as unknown — agents address the
/// trampoline by its full `aether.embedded:NAME` form,
/// which is what `LoadResult.name` returns. Built from
/// The `/`-rendered lineage a loaded component registers at (ADR-0099
/// §4): the component host `aether.component` `/`-joined to the
/// trampoline node — exactly what `LoadResult.name` reports.
fn component_address() -> String {
    use aether_actor::Addressable;
    format!("aether.component/{}:{}", aether_component::WasmTrampoline::NAMESPACE, COMPONENT_NAME)
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
const OUTLINED_PLATE_DSL: &[u8] = b"(box 2 2 0.002 :color 6)\n";

fn loaded_component_address(name: &str) -> String {
    use aether_actor::Addressable;
    format!("aether.component/{}:{name}", aether_component::WasmTrampoline::NAMESPACE)
}

fn load_kit_export(harness: &mut SubstrateHarness, wasm: &[u8], export: &str, name: &str) {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some(name.to_owned()),
                    config: Vec::new(),
                    export: Some(export.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name: address, .. } => {
            assert!(
                address.ends_with(&format!(":{name}")),
                "export {export} should register under :{name}; got {address}"
            );
        }
        LoadResult::Err { error } => panic!("load {export}: {error}"),
    }
}

/// Load `aether-kit-commons`'s pre-built wasm into the harness, selecting the
/// `mesh_viewer` export (ADR-0096; the kit is defaultless per ADR-0138, so
/// the export selector is required), and await `LoadResult`. Panics on load failure so
/// the calling test surfaces the error message rather than wedging on
/// a missing subscription.
fn load_viewer(harness: &mut SubstrateHarness, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    load_kit_export(harness, &wasm, "aether.kit.mesh", COMPONENT_NAME);
}

fn capture_outlined_mesh(
    harness: &mut SubstrateHarness,
    camera_address: &str,
    viewer_address: &str,
    label: &'static str,
) -> Vec<u8> {
    let mails = vec![envelope(camera_address, &Render), envelope(viewer_address, &Render)];
    let captured = harness
        .execute(vec![(label, HarnessOp::capture_with_mails(mails, Vec::new()))])
        .expect("capture outlined mesh");
    captured.captured(label).expect("capture step ran").to_vec()
}

fn is_slate(rgb: &[u8]) -> bool {
    (55..=155).contains(&rgb[0])
        && (55..=155).contains(&rgb[1])
        && (65..=175).contains(&rgb[2])
        && rgb[0].abs_diff(rgb[1]) <= 5
        && rgb[2] >= rgb[0]
}

fn horizontal_outline_thickness(image: &Image) -> u32 {
    let y = image.height / 2;
    let center = image.width / 2;
    let start = center.saturating_sub(24);
    let end = (center + 24).min(image.width.saturating_sub(1));
    let mut longest = 0;
    let mut current = 0;
    for x in start..=end {
        let offset = ((y * image.width + x) * 4) as usize;
        if is_slate(&image.rgba[offset..offset + 3]) {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn vertical_outline_thickness(image: &Image) -> u32 {
    let x = image.width / 2;
    let mut longest = 0;
    let mut current = 0;
    for y in 0..image.height {
        let offset = ((y * image.width + x) * 4) as usize;
        if is_slate(&image.rgba[offset..offset + 3]) {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

#[test]
fn edge_on_outline_stays_visible_and_keeps_apparent_width() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let sandbox = init_save_sandbox("kit-mesh-outline");
    let path = write_fixture("outlined_plate.dsl", OUTLINED_PLATE_DSL);
    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .size(OUTLINE_WINDOW_WIDTH, OUTLINE_WINDOW_HEIGHT)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    load_kit_export(&mut harness, &wasm, "aether.kit.camera", CAMERA_COMPONENT_NAME);
    load_kit_export(&mut harness, &wasm, "aether.kit.mesh", COMPONENT_NAME);
    let camera = loaded_component_address(CAMERA_COMPONENT_NAME);
    let viewer = component_address();
    let loaded = harness
        .execute(vec![
            (
                "aspect",
                HarnessOp::send_and_settle(
                    camera.as_str(),
                    &WindowSize {
                        window: OUTLINE_WINDOW_ID,
                        width: OUTLINE_WINDOW_WIDTH,
                        height: OUTLINE_WINDOW_HEIGHT,
                        scale_factor: 1.0,
                    },
                ),
            ),
            (
                "load_mesh",
                HarnessOp::send_and_await_reply(viewer.as_str(), &LoadMesh { namespace: "save".to_owned(), path }),
            ),
        ])
        .expect("set aspect + load edge-on fixture");
    let reply = loaded.reply::<MeshLoadResult>("load_mesh").expect("decode MeshLoadResult");
    assert!(reply.ok, "edge-on DSL should load: {:?}", reply.error);

    let orbit = |distance, yaw| CameraOrbitSet {
        name: "main".to_owned(),
        params: OrbitParams {
            distance: Some(distance),
            pitch: Some(0.0),
            yaw: Some(yaw),
            speed: Some(0.0),
            fov_y_rad: None,
            target: Some([0.0, 0.0, 0.0]),
        },
    };
    harness
        .execute(vec![("edge_on", HarnessOp::send_and_settle(camera.as_str(), &orbit(4.0, FRAC_PI_2)))])
        .expect("set edge-on orbit");
    let edge_on = capture_outlined_mesh(&mut harness, &camera, &viewer, "edge_on_capture");

    harness
        .execute(vec![("near", HarnessOp::send_and_settle(camera.as_str(), &orbit(2.5, 0.0)))])
        .expect("set near face-on orbit");
    let near = capture_outlined_mesh(&mut harness, &camera, &viewer, "near_capture");

    harness
        .execute(vec![("far", HarnessOp::send_and_settle(camera.as_str(), &orbit(8.0, 0.0)))])
        .expect("set far face-on orbit");
    let far = capture_outlined_mesh(&mut harness, &camera, &viewer, "far_capture");

    let edge_on_image = decode_png(&edge_on).expect("decode edge-on capture");
    let near_image = decode_png(&near).expect("decode near capture");
    let far_image = decode_png(&far).expect("decode far capture");
    let edge_on_thickness = horizontal_outline_thickness(&edge_on_image);
    let near_thickness = vertical_outline_thickness(&near_image);
    let far_thickness = vertical_outline_thickness(&far_image);
    assert!(edge_on_thickness > 0, "the slate outline must remain visible with its authored plate edge-on");
    assert!(near_thickness > 0, "the slate outline must remain visible at the near face-on pose");
    assert!(far_thickness > 0, "the slate outline must remain visible at the far face-on pose");
    assert!(
        near_thickness.abs_diff(far_thickness) <= 1,
        "angular outline width should stay constant within one raster pixel; near={near_thickness}px far={far_thickness}px",
    );
}

/// Assert that `aether.draw_triangle` was observed at least once.
/// Surfaces the observed-kinds list on failure so a typo or missing
/// subscription is debuggable.
fn assert_draw_triangle_observed(harness: &SubstrateHarness) {
    let observed = harness.count_observed("aether.draw_triangle");
    assert!(
        observed >= 1,
        "expected ≥1 aether.draw_triangle observed; got {observed}; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// Smoke test: load a `.dsl` box → triangles flow to the render sink
/// every tick → the captured frame contains pixels that diverge from
/// the chassis clear color. Validates the entire DSL load path: the
/// IO sink read, `aether-mesh`'s parser+mesher, the wireframe outline
/// emission, and the per-tick render-sink replay.
#[test]
fn dsl_box_loads_and_renders() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let sandbox = init_save_sandbox("kit-mesh");
    let path = write_fixture("dsl_box.dsl", BOX_DSL);

    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut harness, &wasm_path);

    // Priming tick triggers the load; the read reply lands on a later
    // tick, so a handful of post-load ticks populate the cache and
    // emit several render-sink frames before the capture.
    let result = harness
        .execute(vec![
            ("prime", HarnessOp::advance(1)),
            (
                "load_mesh",
                HarnessOp::send_and_settle(component_address(), &LoadMesh { namespace: "save".to_owned(), path }),
            ),
            ("post", HarnessOp::advance(5)),
            ("snap", HarnessOp::capture()),
        ])
        .expect("prime + load + advance + capture");

    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    assert_draw_triangle_observed(&harness);
    differs_from_background(&img, 5).expect("captured frame should diverge from clear color");
}

/// `.obj` importer smoke. The shared `aether-mesh` importer supplies indexed
/// triangles and this actor still owns their `DrawTriangle` conversion, so
/// this guards the whole OBJ branch while the DSL branch keeps working.
#[test]
fn obj_quad_loads_and_renders() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let sandbox = init_save_sandbox("kit-mesh");
    let path = write_fixture("obj_quad.obj", QUAD_OBJ);

    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut harness, &wasm_path);

    let result = harness
        .execute(vec![
            ("prime", HarnessOp::advance(1)),
            (
                "load_mesh",
                HarnessOp::send_and_settle(component_address(), &LoadMesh { namespace: "save".to_owned(), path }),
            ),
            ("post", HarnessOp::advance(5)),
            ("snap", HarnessOp::capture()),
        ])
        .expect("prime + load + advance + capture");

    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    assert_draw_triangle_observed(&harness);
    differs_from_background(&img, 5).expect("captured frame should diverge from clear color");
}

/// Parse-failure resilience: a known-bad DSL after a known-good DSL
/// must keep the previous mesh visible — the component's contract is
/// "partial parse / mesh failure leaves the previous mesh intact."
/// Loads a good box, advances until triangles flow, loads the bad
/// DSL, advances again, and verifies the frame still diverges from
/// the clear color.
#[test]
fn parse_failure_keeps_prior_mesh() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let sandbox = init_save_sandbox("kit-mesh");
    let good = write_fixture("good.dsl", BOX_DSL);
    let bad = write_fixture("bad.dsl", BAD_DSL);

    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut harness, &wasm_path);

    harness
        .execute(vec![
            ("prime", HarnessOp::advance(1)),
            (
                "load_good",
                HarnessOp::send_and_settle(component_address(), &LoadMesh { namespace: "save".to_owned(), path: good }),
            ),
            ("post_good", HarnessOp::advance(5)),
        ])
        .expect("prime + good load");

    // Baseline: the good mesh is publishing.
    assert_draw_triangle_observed(&harness);

    // Now hand the viewer something it can't parse, then capture. The
    // cached triangle list should be intact — the frame still has
    // non-clear-color geometry.
    let result = harness
        .execute(vec![
            (
                "load_bad",
                HarnessOp::send_and_settle(component_address(), &LoadMesh { namespace: "save".to_owned(), path: bad }),
            ),
            ("post_bad", HarnessOp::advance(5)),
            ("snap", HarnessOp::capture()),
        ])
        .expect("bad load + capture");
    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    differs_from_background(&img, 5).expect("cached mesh should remain visible after parse failure");
}

/// Issue 964 acceptance: a good-DSL load replies `aether.mesh.load_result`
/// with `ok: true`, no `error`, and no `warnings`, echoing the request's
/// `namespace` + `path`. `send_and_await_reply` blocks through the async
/// `aether.fs.read` round-trip until the structured reply lands, so the
/// reply is the wire signal a harness reads instead of inferring success
/// from rendered geometry.
#[test]
fn good_dsl_load_replies_ok() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let sandbox = init_save_sandbox("kit-mesh");
    let path = write_fixture("reply_good.dsl", BOX_DSL);

    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut harness, &wasm_path);

    let result = harness
        .execute(vec![(
            "load_mesh",
            HarnessOp::send_and_await_reply(
                component_address(),
                &LoadMesh { namespace: "save".to_owned(), path: path.clone() },
            ),
        )])
        .expect("load + reply");

    let reply = result.reply::<MeshLoadResult>("load_mesh").expect("decode MeshLoadResult");
    assert!(reply.ok, "good DSL should load: {:?}", reply.error);
    assert!(reply.error.is_none(), "good load carries no error");
    assert!(reply.warnings.is_empty(), "good load carries no warnings; got {:?}", reply.warnings);
    assert_eq!(reply.namespace, "save", "reply echoes request namespace");
    assert_eq!(reply.path, path, "reply echoes request path");
}

/// Issue 964 acceptance: a bad-DSL load replies `aether.mesh.load_result`
/// with `ok: false` and `error.is_some()`. The prior cache (none here)
/// is untouched; the failure surfaces on the wire rather than only in
/// `engine_logs`.
#[test]
fn bad_dsl_load_replies_err() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let sandbox = init_save_sandbox("kit-mesh");
    let path = write_fixture("reply_bad.dsl", BAD_DSL);

    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut harness, &wasm_path);

    let result = harness
        .execute(vec![(
            "load_mesh",
            HarnessOp::send_and_await_reply(
                component_address(),
                &LoadMesh { namespace: "save".to_owned(), path: path.clone() },
            ),
        )])
        .expect("load + reply");

    let reply = result.reply::<MeshLoadResult>("load_mesh").expect("decode MeshLoadResult");
    assert!(!reply.ok, "bad DSL should not load");
    assert!(reply.error.is_some(), "bad load carries a failure reason");
    assert_eq!(reply.namespace, "save", "reply echoes request namespace");
    assert_eq!(reply.path, path, "reply echoes request path");
}

/// Issue 2796: overlapping mesh loads carry their requester and parse
/// dispatch in request contexts rather than a single actor slot. A second
/// load must not steal the first load's eventual `MeshLoadResult`.
#[test]
fn overlapping_loads_reply_to_their_own_requesters() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let sandbox = init_save_sandbox("kit-mesh");
    let dsl_path = write_fixture("overlap_first.dsl", BOX_DSL);
    let obj_path = write_fixture("overlap_second.obj", QUAD_OBJ);

    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut harness, &wasm_path);

    let first = harness
        .send_deferred(&component_address(), &LoadMesh { namespace: "save".to_owned(), path: dsl_path.clone() })
        .expect("enqueue first mesh load");
    let second = harness
        .send_deferred(&component_address(), &LoadMesh { namespace: "save".to_owned(), path: obj_path.clone() })
        .expect("enqueue second mesh load");

    let second_reply = harness.await_deferred::<MeshLoadResult>(second).expect("second load replies");
    let first_reply = harness.await_deferred::<MeshLoadResult>(first).expect("first load replies");

    assert!(first_reply.ok, "first DSL load should succeed: {:?}", first_reply.error);
    assert!(second_reply.ok, "second OBJ load should succeed: {:?}", second_reply.error);
    assert_eq!(first_reply.path, dsl_path, "first reply keeps its path");
    assert_eq!(second_reply.path, obj_path, "second reply keeps its path");
}
