//! Render-cap harness scenarios (rehomed from `aether-substrate-bundle`'s
//! `substrate_harness_scenario/render.rs`, issue #3771): `capture_frame`
//! pre/after mail bundles, cube projection through a camera, and the
//! ADR-0105/ADR-0140 textured-quad + solid-quad + material surfaces,
//! each driven through an in-process `SubstrateHarness`.
//!
//! Every harness composes exactly the caps its scenario needs (issue
//! #3764): all scenarios compose the render cap via
//! `RenderHarnessBuilderExt::with_render`; the two wasm-loading scenarios
//! (probe round trip, cube projection) add `.with_component_host()`;
//! the similarity scenario adds `.namespace_roots(...)` for the
//! `aether.fs` cap its reference-image lookup reads through.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The fixture's wasm hasn't been built — the wasm-loading tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_test_fixtures_bundle.wasm`
//!   and skip with an `eprintln!` when it's absent. CI builds the
//!   fixture wasm (`cargo xtask dist`) before invoking the tests;
//!   setting `AETHER_REQUIRE_RUNTIME=1` (CI does) flips both skip
//!   points into hard panics so a missing pre-build is loud.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate, `save://` sandbox) live in
//! `aether_harness_substrate_capture::test_helpers` (issues 460 + 821).
//! Per issue 464, the sandbox flows in via
//! `SubstrateHarness::builder().namespace_roots(...)` rather than env-var
//! mutation.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle and the standard
// CARGO_TARGET_DIR build-output override — test-harness knobs, not cap
// config.
#![allow(clippy::disallowed_methods)]

use std::env;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use aether_data::{Kind, MailboxId};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::visual::{
    Image, Rect, background_top_left, bounding_box, centroid, coverage, decode_png, target_color_stats,
};
use aether_harness_substrate_capture::{
    ArtifactGuard, RenderHarnessBuilderExt, RenderHarnessExt,
    test_helpers::{has_wgpu_adapter, init_save_sandbox, require_runtime, test_namespace_roots},
};
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, ClipRect, FrameCheck, FrameCheckResult, FrameRect, FrameReduction, LoadComponent,
    LoadResult, NamedMail, QuadScale, QuadSpace, SimilarityCheck,
};
use aether_math::{Rgb, Rgba};
use aether_render::{
    CreateTexture, CreateTextureResult, DestroyTexture, DrawMaterialCoverage, DrawMaterialTextured, DrawSolidQuads,
    DrawTexturedQuads, DrawTriangle, MaterialCoverageRect, MaterialRect, MaterialTexturedRect, SolidQuad,
    TextureFormat, TexturedQuad, UpdateTexture, Vertex, WHITE_TEXTURE_ID,
};
use aether_substrate::render as substrate_render;
use aether_substrate::render::{QUAD_VERTEX_BUFFER_BYTES, QUAD_VERTEX_STRIDE, QUAD_VERTICES_PER_QUAD};
use aether_test_fixtures_kinds::SetRender;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary. Without the reference, the
// host-target rlib's descriptor symbols can be stripped by the linker
// and `aether_kinds::descriptors::all()` won't see fixture kinds.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

/// Caller-supplied component name passed to `LoadComponent`.
const PROBE_NAME: &str = "probe";
/// Full trampoline address the substrate registers under post-issue-634
/// Phase 4. Mail destined for the loaded probe goes here, not to the
/// bare `PROBE_NAME` (which isn't a registered mailbox). Built from
/// The `/`-rendered lineage a loaded component registers at (ADR-0099
/// §4): the component host `aether.component` `/`-joined to the
/// trampoline node — exactly what `LoadResult.name` reports.
fn probe_address() -> String {
    use aether_actor::Addressable;
    format!("aether.component/{}:{}", aether_component::WasmTrampoline::NAMESPACE, PROBE_NAME)
}

/// Build a `NamedMail` for a `CaptureFrame` mail bundle. Uses
/// the kind's wire encoding (`encode_into_bytes`) so any K — cast
/// or structured — packs correctly.
fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

/// Mirrors `ArtifactGuard`'s private root resolution (`CARGO_MANIFEST_DIR`
/// two levels up to the workspace root, `CARGO_TARGET_DIR` override if
/// set) so the artifact-guard scenario below can locate the directory a
/// real [`ArtifactGuard::arm`] call just wrote to. `id` must already be
/// filesystem-safe (alphanumeric/`-`/`_` only) — the scenario below only
/// ever passes ids it controls, so no sanitization is needed here.
fn artifact_dir(id: &str) -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    let target_root = env::var_os("CARGO_TARGET_DIR").map_or_else(|| workspace.join("target"), PathBuf::from);
    target_root.join("substrate-harness-artifacts").join(id)
}

fn rgba_at(img: &Image, x: u32, y: u32) -> [u8; 4] {
    let start = ((y * img.width + x) * 4) as usize;
    [img.rgba[start], img.rgba[start + 1], img.rgba[start + 2], img.rgba[start + 3]]
}

fn rgb_close(actual: [u8; 4], expected: [u8; 3], tolerance: u8) -> bool {
    actual[..3].iter().zip(expected).all(|(actual, expected)| actual.abs_diff(expected) <= tolerance)
}

fn pixel_is_lit(img: &Image, x: u32, y: u32, bg: [u8; 3], tolerance: u8) -> bool {
    !rgb_close(rgba_at(img, x, y), bg, tolerance)
}

/// Load the probe into the harness via `execute`, blocking on the
/// `LoadResult` reply so subsequent `advance` ops see a
/// fully-instantiated and tick-subscribed component. Returns the
/// loaded component's `MailboxId` (the trampoline address), which
/// the drop / replace scenarios target. Pre-Phase-4 of issue 603 the
/// harness's `aether.control` mailbox (renamed to `aether.component` in
/// issue 638 phase 3) served as a single FIFO point for both load and
/// advance; Phase 4 split advance onto `aether.substrate_harness`, so load is
/// no longer naturally ordered ahead of advance — `SendAndAwaitReply`
/// blocks on `LoadResult` before returning.
fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent { wasm, name: Some(PROBE_NAME.to_owned()), config: Vec::new(), export: None },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// Load the `cube` fixture into the harness, blocking on `LoadResult`
/// so the subsequent advance sees a tick-subscribed component. Mirrors
/// `load_probe`; the cube scenario only needs the load to succeed (it
/// captures rather than mailing the component), so the returned
/// `MailboxId` is discarded.
fn load_cube(harness: &mut SubstrateHarness, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some("test.cube".to_owned()),
                    config: Vec::new(),
                    // `Cube` is a non-entry actor in the bundle.
                    export: Some("test.cube".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component(cube): {error}"),
    }
}

/// Most scenarios here need wgpu (the composed render cap builds a
/// `Gpu` at boot) but not the fixture wasm. Skips on wgpu-less
/// runners and panics under `AETHER_REQUIRE_RUNTIME` so a
/// CI-side regression is loud.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// `capture_frame` round-trip with non-empty mail bundles. The
/// pre-mail bundle flips the fixture's render state to "visible red";
/// the probe then paints one large triangle, so the captured PNG must
/// show a coverage fraction inside a sane band (neither all-background
/// nor all-filled) with a centroid sitting in the frame interior. The
/// after-mail bundle flips render back to invisible; a follow-up
/// advance + plain capture must produce a frame back at the clear
/// color — near-zero coverage — proving the after-mail cleanup ran.
#[test]
#[allow(clippy::cast_precision_loss)]
fn capture_frame_round_trip_runs_pre_and_after_mails() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");
    load_probe(&mut harness, &wasm_path);

    // Capture's frame runs without a dispatched tick, so the probe
    // won't auto-tick during the captured frame. The pre-mail bundle
    // wires it up: `set_render` flips state to "visible red", and a
    // synthesised `aether.lifecycle.tick` drives the probe's on_tick
    // to emit a `DrawTriangle` into the frame buffer right before the
    // GPU readback. The after-mail bundle flips render back to
    // invisible after the readback.
    let pre = vec![
        envelope(&probe_address(), &SetRender { r: 200, g: 32, b: 32, visible: 1 }),
        NamedMail {
            recipient_name: probe_address(),
            kind_name: "aether.lifecycle.tick".to_owned(),
            payload: Vec::new(),
            count: 1,
        },
    ];
    let after = vec![envelope(&probe_address(), &SetRender { r: 0, g: 0, b: 0, visible: 0 })];

    // Priming advance subscribes the probe to ticks; the
    // capture-with-mails op then dispatches the pre bundle, reads
    // back, and dispatches the after bundle — all in one frame.
    let captured = harness
        .execute(vec![("prime", HarnessOp::advance(1)), ("snap", HarnessOp::capture_with_mails(pre, after))])
        .expect("prime + capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;
    // The probe draws one large triangle (NDC verts spanning ±0.9),
    // covering roughly 40% of the frame. A coverage band rules out the
    // two ways the old single-pixel `differs_from_background` check went
    // placebo: an all-background miss (drew nothing) and an all-filled
    // frame (clear color itself diverging from the sampled corner).
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.05..0.95).contains(&drawn),
        "probe triangle coverage {drawn} fell outside the expected band (0.05, 0.95); \
         the captured frame is effectively empty or entirely filled",
    );
    // The triangle is centered on the middle column and weighted toward
    // the lower half, so its centroid lands well inside the frame rather
    // than hugging an edge.
    let (center_x, center_y) = centroid(&img, bg, tolerance).expect("a lit frame has a centroid");
    let (width, height) = (img.width as f32, img.height as f32);
    assert!(
        center_x > 0.1 * width && center_x < 0.9 * width && center_y > 0.1 * height && center_y < 0.9 * height,
        "triangle centroid ({center_x}, {center_y}) should sit in the frame interior \
         of the {}x{} capture",
        img.width,
        img.height,
    );

    // Cleanup ran: probe.render is now { visible: 0 }. Advance once
    // and capture again — the next tick won't emit DrawTriangle, so
    // the frame stays at clear color.
    let cleaned = harness
        .execute(vec![("cleanup_advance", HarnessOp::advance(1)), ("snap2", HarnessOp::capture())])
        .expect("post-cleanup advance + capture");
    let png2 = cleaned.captured("snap2").expect("snap2 step ran");
    let img2 = decode_png(png2).expect("decode cleanup png");
    let cleaned_coverage = coverage(&img2, background_top_left(&img2), 5);
    assert!(
        cleaned_coverage < 0.01,
        "after after-mail cleanup the captured frame should be uniform clear color, \
         but coverage was {cleaned_coverage} (cleanup did not run)",
    );
}

/// Render-pipeline proof: load the `cube` fixture, drive one tick, and
/// capture. The fixture publishes a fixed `ViewProjection { view_proj }` and a
/// twelve-triangle world-space unit cube, so the captured frame puts
/// every stage on the line at once — camera, `view_proj`, world-space
/// geometry, the depth test that orders the cube's faces, and GPU
/// readback. The existing `capture_frame_round_trip` scenario only
/// draws a flat NDC triangle at identity `view_proj`, so this is the
/// first capture that actually projects geometry through a camera.
///
/// The assertions use the #1513 silhouette reductions against the
/// known framing matrix: the cube's lit bounding box must sit centered
/// and inset from the frame edges (not a corner speck, not full-bleed),
/// and coverage must land in the cube's band. The bounds below were
/// tuned against the real captured frame at this size and `view_proj`.
#[test]
#[allow(clippy::cast_precision_loss)]
fn cube_render_projects_centered_silhouette() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    // 128×96 matches the fixture's `view_proj` aspect (4:3), so the
    // silhouette projects undistorted.
    let (width, height) = (128u32, 96u32);
    let mut harness =
        SubstrateHarness::builder().size(width, height).with_render().with_component_host().build().expect("boot");
    load_cube(&mut harness, &wasm_path);

    // Priming advance subscribes the cube to ticks; the next tick (run
    // inside `capture`) drives the cube's camera + geometry emission so
    // the readback sees a fully-formed frame.
    let captured = harness
        .execute(vec![("prime", HarnessOp::advance(1)), ("snap", HarnessOp::capture())])
        .expect("prime + capture");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // Coverage band: the cube fills a healthy fraction of the frame but
    // leaves the clear color showing in the corners. The fixed
    // `view_proj` makes this deterministic; the observed fraction is
    // ~0.18, so the band brackets it with margin while still ruling out
    // an empty frame (drew nothing) and a full-bleed frame (clear-color
    // mismatch or runaway geometry).
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.10..0.30).contains(&drawn),
        "cube coverage {drawn} fell outside the expected band (0.10, 0.30); \
         the captured frame is effectively empty or entirely filled",
    );

    // The silhouette must be centered and inset from every edge —
    // proving the cube projected to the middle of the frame, not into a
    // corner and not bleeding past the borders.
    let silhouette = bounding_box(&img, bg, tolerance).expect("a lit frame has a bounding box");
    let (frame_width, frame_height) = (img.width as f32, img.height as f32);
    let min_x = silhouette.min_x as f32;
    let min_y = silhouette.min_y as f32;
    let max_x = silhouette.max_x as f32;
    let max_y = silhouette.max_y as f32;
    assert!(
        min_x > 0.05 * frame_width
            && max_x < 0.95 * frame_width
            && min_y > 0.05 * frame_height
            && max_y < 0.95 * frame_height,
        "cube silhouette {silhouette:?} should be inset from the edges of the \
         {}x{} frame (not full-bleed)",
        img.width,
        img.height,
    );
    assert!(
        min_x < 0.45 * frame_width
            && max_x > 0.55 * frame_width
            && min_y < 0.45 * frame_height
            && max_y > 0.55 * frame_height,
        "cube silhouette {silhouette:?} should straddle the center of the \
         {}x{} frame (not a corner speck)",
        img.width,
        img.height,
    );
}

/// ADR-0105 textured-quad surface: create an RGBA8 texture from raw
/// pixels, draw a `Screen`-space quad sampling it at a known pixel rect,
/// and assert the captured frame lights that rect. A second capture
/// after an advance with no resent quads asserts the immediate-mode
/// clear — the quad disappears, matching `aether.draw_triangle`.
///
/// No component is loaded; the quad is the only thing that can light a
/// pixel, so the silhouette reductions pin it directly. The pre-mail
/// bundle dispatches the `draw_textured_quads` into the accumulator
/// right before the readback, the same way the probe scenario
/// synthesises a tick.
#[test]
#[allow(clippy::cast_precision_loss)]
fn textured_quad_draws_screen_space_rect() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");

    // 8×8 checkerboard of opaque white and opaque red — both far from the
    // dark clear color, so every magnified texel of the quad reads as lit
    // regardless of which cell it samples.
    let texture_width = 8u32;
    let texture_height = 8u32;
    let mut pixels = Vec::with_capacity((texture_width * texture_height * 4) as usize);
    for y in 0..texture_height {
        for x in 0..texture_width {
            let white = (x / 2 + y / 2) % 2 == 0;
            if white {
                pixels.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                pixels.extend_from_slice(&[255, 0, 0, 255]);
            }
        }
    }

    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture { width: texture_width, height: texture_height, format: TextureFormat::Rgba8, pixels },
            ),
        )])
        .expect("create_texture sequence");
    let texture_id = match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    // Known screen rect: top-left (16, 12), size 24×18 → columns 16..40,
    // rows 12..30. Rasterized pixel centers give an inclusive lit box of
    // roughly [16, 39] × [12, 29].
    let (quad_x, quad_y, quad_w, quad_h) = (16.0f32, 12.0f32, 24.0f32, 18.0f32);
    let pre = vec![envelope(
        "aether.render",
        &DrawTexturedQuads {
            texture_id,
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![TexturedQuad {
                x: quad_x,
                y: quad_y,
                width: quad_w,
                height: quad_h,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        },
    )];

    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // Coverage band around the quad's area fraction (24*18 / 64*48 ≈
    // 0.14) — rules out an empty frame and a full-bleed frame.
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.08..0.22).contains(&drawn),
        "quad coverage {drawn} fell outside the expected band (0.08, 0.22); \
         the captured frame is effectively empty or entirely filled",
    );

    // The lit box must land on the requested rect — proving the
    // screen-space ortho mapped pixels (16, 12)–(40, 30) to the frame.
    let silhouette = bounding_box(&img, bg, tolerance).expect("a lit frame has a bounding box");
    assert!(
        (14..=18).contains(&silhouette.min_x)
            && (37..=41).contains(&silhouette.max_x)
            && (10..=14).contains(&silhouette.min_y)
            && (27..=31).contains(&silhouette.max_y),
        "quad silhouette {silhouette:?} should bound the screen rect (16,12)-(40,30) \
         of the {frame_width}x{frame_height} frame",
    );

    // Immediate-mode contract: with no quad resent, an advance commits
    // the empty accumulator (clearing the cache) and the next capture is
    // back at clear color.
    let cleared = harness
        .execute(vec![("clear_advance", HarnessOp::advance(1)), ("snap2", HarnessOp::capture())])
        .expect("advance + capture");
    let png2 = cleared.captured("snap2").expect("snap2 step ran");
    let img2 = decode_png(png2).expect("decode cleared png");
    let cleared_coverage = coverage(&img2, background_top_left(&img2), tolerance);
    assert!(
        cleared_coverage < 0.01,
        "after the quad stopped being sent the frame should be uniform clear color, \
         but coverage was {cleared_coverage} (immediate-mode clear did not run)",
    );
}

fn create_observation_texture(harness: &mut SubstrateHarness) -> u32 {
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture { width: 1, height: 1, format: TextureFormat::Rgba8, pixels: vec![255, 255, 255, 255] },
            ),
        )])
        .expect("create observation texture");
    match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    }
}

fn assert_committed_overlay_snapshot(
    snapshot: &[DrawTexturedQuads],
    texture_id: u32,
    solid_clip: ClipRect,
    solid_quad: &SolidQuad,
    textured_space: &QuadSpace,
    textured_clip: ClipRect,
    textured_quad: TexturedQuad,
) {
    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[0].texture_id, WHITE_TEXTURE_ID);
    assert_eq!(snapshot[0].space, QuadSpace::Screen);
    assert_eq!(snapshot[0].clip, Some(solid_clip));
    assert_eq!(snapshot[0].quads.len(), 1);
    assert_eq!(snapshot[0].quads[0].x, solid_quad.x);
    assert_eq!(snapshot[0].quads[0].y, solid_quad.y);
    assert_eq!(snapshot[0].quads[0].width, solid_quad.width);
    assert_eq!(snapshot[0].quads[0].height, solid_quad.height);
    assert_eq!(snapshot[0].quads[0].u0, 0.0);
    assert_eq!(snapshot[0].quads[0].v0, 0.0);
    assert_eq!(snapshot[0].quads[0].u1, 1.0);
    assert_eq!(snapshot[0].quads[0].v1, 1.0);
    assert_eq!(snapshot[0].quads[0].tint, solid_quad.color);

    assert_eq!(snapshot[1].texture_id, texture_id);
    assert_eq!(&snapshot[1].space, textured_space);
    assert_eq!(snapshot[1].clip, Some(textured_clip));
    assert_eq!(snapshot[1].quads, vec![textured_quad]);
}

/// Typed overlay observations expose the exact normalized batches the
/// committed frame draws: a solid batch becomes a textured batch over the
/// reserved white texture, a following textured batch keeps its own texture,
/// and order, spaces, clips, geometry, UVs, and tints all survive. An idle
/// capture replays that cache, while the next empty advance clears it.
#[test]
fn committed_overlay_snapshot_is_typed_ordered_and_latest_frame_bounded() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");
    let texture_id = create_observation_texture(&mut harness);

    let solid_clip = ClipRect { x: 2.0, y: 3.0, width: 20.0, height: 15.0 };
    let solid_quad = SolidQuad { x: 4.0, y: 5.0, width: 6.0, height: 7.0, color: Rgba::new(0.9, 0.2, 0.3, 0.8) };
    let textured_space =
        QuadSpace::World { anchor: [0.25, -0.5, 0.75], scale: QuadScale::Distance { reference_distance: 4.0 } };
    let textured_clip = ClipRect { x: 10.0, y: 11.0, width: 30.0, height: 25.0 };
    let textured_quad = TexturedQuad {
        x: -8.0,
        y: -9.0,
        width: 12.0,
        height: 13.0,
        u0: 0.1,
        v0: 0.2,
        u1: 0.7,
        v1: 0.8,
        tint: Rgba::new(0.1, 0.4, 0.7, 0.6),
    };
    let submissions = vec![
        envelope(
            "aether.render",
            &DrawSolidQuads {
                space: QuadSpace::Screen,
                clip: Some(solid_clip.clone()),
                quads: vec![solid_quad.clone()],
            },
        ),
        envelope(
            "aether.render",
            &DrawTexturedQuads {
                texture_id,
                space: textured_space.clone(),
                clip: Some(textured_clip.clone()),
                quads: vec![textured_quad.clone()],
            },
        ),
    ];

    harness
        .execute(vec![("commit", HarnessOp::capture_with_mails(submissions, vec![]))])
        .expect("commit overlay submissions through capture");
    let snapshot = harness.committed_overlay_snapshot();
    assert_committed_overlay_snapshot(
        &snapshot,
        texture_id,
        solid_clip,
        &solid_quad,
        &textured_space,
        textured_clip,
        textured_quad,
    );

    harness.execute(vec![("replay", HarnessOp::capture())]).expect("idle capture replays committed overlays");
    assert_eq!(harness.committed_overlay_snapshot().len(), 2);

    harness.execute(vec![("clear", HarnessOp::advance(1))]).expect("commit subsequent empty frame");
    assert!(harness.committed_overlay_snapshot().is_empty());
}

/// Observation follows record-time rejection, not the raw submission cache:
/// empty and offscreen-clipped batches disappear individually, while an
/// aggregate vertex-buffer overflow drops the whole pass from both the
/// structural snapshot and rendered frame.
#[test]
fn committed_overlay_snapshot_excludes_record_time_rejections() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");
    let texture_id = create_observation_texture(&mut harness);
    let valid_quad = TexturedQuad {
        x: 16.0,
        y: 12.0,
        width: 24.0,
        height: 18.0,
        u0: 0.0,
        v0: 0.0,
        u1: 1.0,
        v1: 1.0,
        tint: Rgba::new(1.0, 0.2, 0.1, 1.0),
    };
    let valid_batch =
        DrawTexturedQuads { texture_id, space: QuadSpace::Screen, clip: None, quads: vec![valid_quad.clone()] };
    let submissions = vec![
        envelope(
            "aether.render",
            &DrawTexturedQuads { texture_id, space: QuadSpace::Screen, clip: None, quads: Vec::new() },
        ),
        envelope(
            "aether.render",
            &DrawTexturedQuads {
                texture_id,
                space: QuadSpace::Screen,
                clip: Some(ClipRect { x: 74.0, y: 0.0, width: 5.0, height: 5.0 }),
                quads: vec![valid_quad.clone()],
            },
        ),
        envelope("aether.render", &valid_batch),
    ];
    let captured = harness
        .execute(vec![("filtered", HarnessOp::capture_with_mails(submissions, vec![]))])
        .expect("capture valid and individually rejected batches");
    let snapshot = harness.committed_overlay_snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].texture_id, valid_batch.texture_id);
    assert_eq!(snapshot[0].space, valid_batch.space);
    assert_eq!(snapshot[0].clip, valid_batch.clip);
    assert_eq!(snapshot[0].quads, valid_batch.quads);
    let filtered =
        decode_png(captured.captured("filtered").expect("filtered capture")).expect("decode filtered capture");
    let filtered_coverage = coverage(&filtered, background_top_left(&filtered), 5);
    assert!(
        (0.08..0.22).contains(&filtered_coverage),
        "only the valid quad should render, coverage was {filtered_coverage}",
    );

    let bytes_per_quad =
        usize::try_from(QUAD_VERTEX_STRIDE).expect("quad vertex stride fits usize") * QUAD_VERTICES_PER_QUAD;
    let over_budget_count = QUAD_VERTEX_BUFFER_BYTES / bytes_per_quad + 1;
    let oversized = DrawTexturedQuads {
        texture_id,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![valid_quad; over_budget_count],
    };
    let overflow = harness
        .execute(vec![("overflow", HarnessOp::capture_with_mails(vec![envelope("aether.render", &oversized)], vec![]))])
        .expect("capture over-budget overlay pass");
    assert!(harness.committed_overlay_snapshot().is_empty());
    let overflow =
        decode_png(overflow.captured("overflow").expect("overflow capture")).expect("decode overflow capture");
    let overflow_coverage = coverage(&overflow, background_top_left(&overflow), 5);
    assert!(overflow_coverage < 0.01, "over-budget pass should render nothing, coverage was {overflow_coverage}");
}

/// Palette for the four-quadrant texture built by
/// `four_quadrant_texture_pixels` and probed by
/// `target_color_stats_distinguishes_quadrant_colors_on_real_capture`.
const QUADRANT_RED: [u8; 3] = [255, 0, 0];
const QUADRANT_GREEN: [u8; 3] = [0, 255, 0];
const QUADRANT_BLUE: [u8; 3] = [0, 0, 255];
const QUADRANT_YELLOW: [u8; 3] = [255, 255, 0];

/// Build a `size x size` opaque RGBA8 texture split into four solid
/// color quadrants: red (top-left), green (top-right), blue
/// (bottom-left), yellow (bottom-right).
fn four_quadrant_texture_pixels(size: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let color = match (x < size / 2, y < size / 2) {
                (true, true) => QUADRANT_RED,
                (false, true) => QUADRANT_GREEN,
                (true, false) => QUADRANT_BLUE,
                (false, false) => QUADRANT_YELLOW,
            };
            pixels.extend_from_slice(&[color[0], color[1], color[2], 255]);
        }
    }
    pixels
}

/// Assert `target_color_stats` reports a high matching fraction for
/// `target` within `region`, with a centroid landing inside that same
/// region — the per-quadrant assertion shared by
/// `target_color_stats_distinguishes_quadrant_colors_on_real_capture`.
#[allow(clippy::cast_precision_loss)]
fn assert_quadrant_matches(img: &Image, label: &str, region: Rect, target: [u8; 3], tolerance: u8) {
    let stats = target_color_stats(img, target, tolerance, Some(region));
    assert!(
        stats.fraction > 0.8,
        "{label} quadrant probe matched {target:?} at fraction {}, expected > 0.8 \
         (region {region:?})",
        stats.fraction,
    );
    let center = stats.centroid.expect("high-fraction probe has a centroid");
    assert!(
        (region.min_x as f32..=region.max_x as f32).contains(&center.x)
            && (region.min_y as f32..=region.max_y as f32).contains(&center.y),
        "{label} centroid ({}, {}) should sit inside its own probe \
         region {region:?}",
        center.x,
        center.y,
    );
    assert_eq!(
        stats.bounding_box,
        Some(region),
        "{label} target-color extent should exactly recover its inset probe region",
    );
}

/// Prerequisite for issue #2912's `target_color_stats`: create a
/// four-quadrant RGBA8 texture, draw it as a known `Screen`-space quad,
/// decode the capture, and probe an inset rect in each quadrant. Unlike
/// `textured_quad_draws_screen_space_rect` (which only proves
/// *something* lit the requested rect against a dark background), this
/// proves the color-aware probe: the intended color owns a high
/// matching fraction with a bounded centroid inside its own quadrant,
/// while the same target has near-zero matches in a neighboring
/// quadrant that holds a different color. Probe rects are inset from
/// every quadrant edge (including the internal seams) so
/// linear-filtered boundary texels never fall inside a probed region.
#[test]
fn target_color_stats_distinguishes_quadrant_colors_on_real_capture() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");

    let texture_size = 8u32;
    let pixels = four_quadrant_texture_pixels(texture_size);
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture { width: texture_size, height: texture_size, format: TextureFormat::Rgba8, pixels },
            ),
        )])
        .expect("create_texture sequence");
    let texture_id = match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    // Screen rect (16, 12) sized 32x24: columns 16..48, rows 12..36,
    // split at the midlines x=32 / y=24 into four 16x12 quadrants
    // matching the texture's u/v split.
    let pre = vec![envelope(
        "aether.render",
        &DrawTexturedQuads {
            texture_id,
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![TexturedQuad {
                x: 16.0,
                y: 12.0,
                width: 32.0,
                height: 24.0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        },
    )];

    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let tolerance = 20;

    // Inset 8x4 probe rects, one per quadrant, each pulled at least 4px
    // back from its quadrant's outer edges and the internal x=32/y=24
    // seams so no linear-filtered boundary texel falls inside a probe.
    let top_left = Rect { min_x: 20, min_y: 16, max_x: 27, max_y: 19 };
    let top_right = Rect { min_x: 36, min_y: 16, max_x: 43, max_y: 19 };
    let bottom_left = Rect { min_x: 20, min_y: 28, max_x: 27, max_y: 31 };
    let bottom_right = Rect { min_x: 36, min_y: 28, max_x: 43, max_y: 31 };

    assert_quadrant_matches(&img, "top-left", top_left, QUADRANT_RED, tolerance);
    assert_quadrant_matches(&img, "top-right", top_right, QUADRANT_GREEN, tolerance);
    assert_quadrant_matches(&img, "bottom-left", bottom_left, QUADRANT_BLUE, tolerance);
    assert_quadrant_matches(&img, "bottom-right", bottom_right, QUADRANT_YELLOW, tolerance);

    // Cross-check: the top-left region's own color (red) does not
    // appear in the top-right region, which holds green.
    let cross = target_color_stats(&img, QUADRANT_RED, tolerance, Some(top_right));
    assert!(
        cross.fraction < 0.1,
        "red target matched {} fraction of the top-right (green) quadrant probe, \
         expected < 0.1",
        cross.fraction,
    );
}

/// Issue #2831: a destroyed texture is removed from the registry, so a
/// later draw using the old id warn-drops during frame record and the
/// captured frame returns to clear color.
#[test]
#[allow(clippy::cast_precision_loss)]
fn destroyed_texture_draw_drops_from_frame() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");

    let texture_width = 8u32;
    let texture_height = 8u32;
    let pixels = vec![255u8; (texture_width * texture_height * 4) as usize];
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture { width: texture_width, height: texture_height, format: TextureFormat::Rgba8, pixels },
            ),
        )])
        .expect("create_texture sequence");
    let texture_id = match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    let draw = || {
        envelope(
            "aether.render",
            &DrawTexturedQuads {
                texture_id,
                space: QuadSpace::Screen,
                clip: None,
                quads: vec![TexturedQuad {
                    x: 16.0,
                    y: 12.0,
                    width: 24.0,
                    height: 18.0,
                    u0: 0.0,
                    v0: 0.0,
                    u1: 1.0,
                    v1: 1.0,
                    tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
                }],
            },
        )
    };

    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![draw()], vec![]))])
        .expect("capture with live texture");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let drawn = coverage(&img, bg, 5);
    assert!((0.08..0.22).contains(&drawn), "live texture quad coverage {drawn} fell outside the expected band");

    let destroyed = harness
        .execute(vec![
            ("destroy", HarnessOp::send_and_settle("aether.render", &DestroyTexture { texture_id })),
            ("advance", HarnessOp::advance(1)),
            ("snap2", HarnessOp::capture_with_mails(vec![draw()], vec![])),
        ])
        .expect("destroy texture and capture same draw next frame");
    let png2 = destroyed.captured("snap2").expect("snap2 step ran");
    let img2 = decode_png(png2).expect("decode destroyed capture png");
    let destroyed_coverage = coverage(&img2, background_top_left(&img2), 5);
    assert!(
        destroyed_coverage < 0.01,
        "after destroy the same draw should drop from the frame, but coverage was \
         {destroyed_coverage}",
    );
    assert!(
        harness.committed_overlay_snapshot().is_empty(),
        "the typed observation must reject the same missing-texture batch as the raster pass",
    );
}

/// ADR-0140 texture-format half: an R8 texture stages one byte per
/// pixel, accepts one-byte sub-rect updates, realizes as a sampleable
/// `R8Unorm` texture, and renders through the existing textured-quad
/// shader as red-channel-only (`vec4(r, 0, 0, 1)`).
#[test]
fn r8_texture_updates_and_draws_red_channel_only() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");

    let texture_width = 8u32;
    let texture_height = 4u32;
    let mut pixels = vec![32u8; (texture_width * texture_height) as usize];
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture {
                    width: texture_width,
                    height: texture_height,
                    format: TextureFormat::R8,
                    pixels: pixels.clone(),
                },
            ),
        )])
        .expect("create r8 texture");
    let texture_id = match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    let update_width = texture_width / 2;
    let update_height = texture_height;
    pixels.clear();
    pixels.resize((update_width * update_height) as usize, 224);

    let pre = vec![
        envelope(
            "aether.render",
            &UpdateTexture { texture_id, x: update_width, y: 0, width: update_width, height: update_height, pixels },
        ),
        envelope(
            "aether.render",
            &DrawTexturedQuads {
                texture_id,
                space: QuadSpace::Screen,
                clip: None,
                quads: vec![TexturedQuad {
                    x: 16.0,
                    y: 16.0,
                    width: 32.0,
                    height: 16.0,
                    u0: 0.0,
                    v0: 0.0,
                    u1: 1.0,
                    v1: 1.0,
                    tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
                }],
            },
        ),
    ];

    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture r8 texture");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    assert_eq!((img.width, img.height), (frame_width, frame_height));

    let sample = |x: u32, y: u32| -> [u8; 4] {
        let start = ((y * img.width + x) * 4) as usize;
        [img.rgba[start], img.rgba[start + 1], img.rgba[start + 2], img.rgba[start + 3]]
    };
    let left = sample(20, 24);
    let right = sample(44, 24);

    assert!(
        right[0] > left[0].saturating_add(80),
        "right-half R8 update should visibly raise only red; left={left:?} right={right:?}",
    );
    assert!(
        left[1] <= 10 && left[2] <= 10 && right[1] <= 10 && right[2] <= 10,
        "R8 texture sampled through quad shader should not contribute green/blue; \
         left={left:?} right={right:?}",
    );
}

/// ADR-0140 coverage material: an R8 plane renders in the world-space
/// material pass between the main pass and overlay. A hand-authored
/// horizontal coverage field produces outside/body/rim samples at known
/// pixels.
#[test]
fn coverage_material_renders_body_rim_and_outside_bands() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");
    let pixels = vec![
        0, 0, 0, 0, 128, 128, 255, 255, //
        0, 0, 0, 0, 128, 128, 255, 255, //
        0, 0, 0, 0, 128, 128, 255, 255, //
        0, 0, 0, 0, 128, 128, 255, 255,
    ];
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture { width: 8, height: 4, format: TextureFormat::R8, pixels },
            ),
        )])
        .expect("create coverage texture");
    let texture_id = match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    let pre = vec![envelope(
        "aether.render",
        &DrawMaterialCoverage {
            texture_id,
            rects: vec![MaterialCoverageRect {
                rect: MaterialRect { x: -0.8, y: -0.6, width: 1.6, height: 1.2, z: 0.5 },
                body_color: Rgba::new(0.0, 0.9, 0.1, 1.0),
                rim_color: Rgba::new(1.0, 0.9, 0.0, 1.0),
                rim_width: 0.25,
            }],
        },
    )];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture coverage material");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode coverage material png");
    let bg = background_top_left(&img);
    let outside = rgba_at(&img, 12, 24);
    let rim = rgba_at(&img, 38, 24);
    let body = rgba_at(&img, 48, 24);

    assert!(
        outside[0].abs_diff(bg[0]) <= 8 && outside[1].abs_diff(bg[1]) <= 8 && outside[2].abs_diff(bg[2]) <= 8,
        "outside coverage sample should stay background; bg={bg:?} outside={outside:?}",
    );
    assert!(rim[0] > 150 && rim[1] > 120 && rim[2] < 80, "coverage rim sample should be yellow; got {rim:?}");
    assert!(
        body[1] > body[0].saturating_add(80) && body[1] > body[2].saturating_add(60),
        "coverage body sample should be green; got {body:?}",
    );
}

/// ADR-0140 textured material: a world-space RGBA8 material rect samples
/// a texture and depth-tests against the main pass. The left half is
/// covered by a main-pass triangle at a nearer depth, while the right
/// half remains visible.
#[test]
fn textured_material_depth_tests_against_main_geometry() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");
    let pixels = vec![255u8, 255, 255, 255];
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture { width: 1, height: 1, format: TextureFormat::Rgba8, pixels },
            ),
        )])
        .expect("create textured material texture");
    let texture_id = match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    let occluder = DrawTriangle {
        verts: [
            Vertex { x: -0.9, y: -0.8, z: 0.0, color: Rgb::new(0.9, 0.0, 0.0) },
            Vertex { x: -0.9, y: 0.8, z: 0.0, color: Rgb::new(0.9, 0.0, 0.0) },
            Vertex { x: 0.0, y: 0.8, z: 0.0, color: Rgb::new(0.9, 0.0, 0.0) },
        ],
    };
    let pre = vec![
        envelope("aether.render", &occluder),
        envelope(
            "aether.render",
            &DrawMaterialTextured {
                texture_id,
                rects: vec![MaterialTexturedRect {
                    rect: MaterialRect { x: -0.8, y: -0.6, width: 1.6, height: 1.2, z: 0.5 },
                    u0: 0.0,
                    v0: 0.0,
                    u1: 1.0,
                    v1: 1.0,
                    tint: Rgba::new(0.0, 0.1, 1.0, 1.0),
                }],
            },
        ),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture textured material");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode textured material png");
    let left = rgba_at(&img, 12, 20);
    let right = rgba_at(&img, 48, 24);
    assert!(
        left[0] > left[2].saturating_add(80),
        "left sample should show red main-pass occluder, not blue material; got {left:?}",
    );
    assert!(right[2] > right[0].saturating_add(100), "right sample should show blue textured material; got {right:?}");
}

/// ADR-0140 coverage material rejects non-R8 textures at encode time:
/// the batch warn-drops, the frame still renders, and no material pixels
/// appear.
#[test]
fn coverage_material_warn_drops_non_r8_texture() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture { width: 2, height: 2, format: TextureFormat::Rgba8, pixels: vec![255u8; 16] },
            ),
        )])
        .expect("create rgba texture");
    let texture_id = match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };
    let pre = vec![envelope(
        "aether.render",
        &DrawMaterialCoverage {
            texture_id,
            rects: vec![MaterialCoverageRect {
                rect: MaterialRect { x: -0.8, y: -0.6, width: 1.6, height: 1.2, z: 0.5 },
                body_color: Rgba::new(0.0, 1.0, 0.0, 1.0),
                rim_color: Rgba::new(1.0, 1.0, 0.0, 1.0),
                rim_width: 0.25,
            }],
        },
    )];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture non-r8 coverage");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode non-r8 coverage png");
    let drawn = coverage(&img, background_top_left(&img), 5);
    assert!(drawn < 0.01, "coverage draw against RGBA8 should be warn-dropped, but lit coverage was {drawn}");
}

/// ADR-0107 §4 flat-fill primitive: a `draw_solid_quads` batch draws an
/// opaque screen-space rect in the overlay pass without a caller-created
/// texture. The test dispatches a single `SolidQuad` covering a known
/// pixel rect and asserts `coverage > 0` and `centroid` inside the rect.
/// A second capture after an advance with no resent quads asserts the
/// immediate-mode clear — exactly the same contract as
/// `textured_quad_draws_screen_space_rect`.
#[test]
#[allow(clippy::cast_precision_loss)]
fn solid_quad_draws_screen_space_rect() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");

    // Known screen rect: top-left (16, 12), size 24×18.
    let (quad_x, quad_y, quad_w, quad_h) = (16.0f32, 12.0f32, 24.0f32, 18.0f32);
    let pre = vec![envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![SolidQuad {
                x: quad_x,
                y: quad_y,
                width: quad_w,
                height: quad_h,
                color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        },
    )];

    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // Coverage band around the quad's area fraction (24*18 / 64*48 ≈ 0.14).
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.08..0.22).contains(&drawn),
        "solid quad coverage {drawn} fell outside the expected band (0.08, 0.22); \
         the captured frame is effectively empty or entirely filled",
    );

    // The lit centroid must land inside the requested rect — ruling out a misplaced fill.
    let (cx, cy) = centroid(&img, bg, tolerance).expect("a lit frame has a centroid");
    let pad = 4.0f32;
    assert!(
        cx >= quad_x - pad && cx <= quad_x + quad_w + pad && cy >= quad_y - pad && cy <= quad_y + quad_h + pad,
        "solid quad centroid ({cx}, {cy}) should sit inside the screen rect \
         ({quad_x},{quad_y})+({quad_w}x{quad_h}) of the {frame_width}x{frame_height} frame",
    );

    // Immediate-mode clear: advance with no quad resent, next capture returns to clear color.
    let cleared = harness
        .execute(vec![("clear_advance", HarnessOp::advance(1)), ("snap2", HarnessOp::capture())])
        .expect("advance + capture");
    let png2 = cleared.captured("snap2").expect("snap2 step ran");
    let img2 = decode_png(png2).expect("decode cleared png");
    let cleared_coverage = coverage(&img2, background_top_left(&img2), tolerance);
    assert!(
        cleared_coverage < 0.01,
        "after the solid quad stopped being sent the frame should be uniform clear color, \
         but coverage was {cleared_coverage} (immediate-mode clear did not run)",
    );
}

/// Issue #2855: a per-batch clip rect becomes a GPU scissor. A clipped
/// solid batch can only light pixels inside the clip, and the following
/// unclipped batch resets to the full framebuffer instead of inheriting
/// the prior scissor.
#[test]
fn solid_quad_clip_bounds_pixels_and_does_not_leak() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");
    let clipped = envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: Some(ClipRect { x: 20.0, y: 12.0, width: 12.0, height: 10.0 }),
            quads: vec![SolidQuad { x: 10.0, y: 8.0, width: 44.0, height: 30.0, color: Rgba::new(1.0, 0.0, 0.0, 1.0) }],
        },
    );
    let unclipped = envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![SolidQuad { x: 44.0, y: 30.0, width: 8.0, height: 8.0, color: Rgba::new(0.0, 1.0, 0.0, 1.0) }],
        },
    );

    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![clipped, unclipped], vec![]))])
        .expect("capture clipped solid quads");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode clipped solid png");
    let bg = background_top_left(&img);
    let tolerance = 5;
    assert!(pixel_is_lit(&img, 24, 16, bg, tolerance), "pixel inside the solid clip rect should be painted");
    assert!(
        !pixel_is_lit(&img, 16, 16, bg, tolerance),
        "pixel inside the solid quad but outside the clip rect should remain clear",
    );
    assert!(
        pixel_is_lit(&img, 48, 34, bg, tolerance),
        "following unclipped batch should paint outside the previous clip rect",
    );
}

/// Issue #2855: user-textured quad batches carry the same per-call
/// framebuffer clip as solid batches.
#[test]
fn textured_quad_clip_bounds_pixels() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CreateTexture { width: 1, height: 1, format: TextureFormat::Rgba8, pixels: vec![255, 255, 255, 255] },
            ),
        )])
        .expect("create white texture");
    let texture_id = match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };
    let draw = envelope(
        "aether.render",
        &DrawTexturedQuads {
            texture_id,
            space: QuadSpace::Screen,
            clip: Some(ClipRect { x: 18.0, y: 14.0, width: 14.0, height: 12.0 }),
            quads: vec![TexturedQuad {
                x: 8.0,
                y: 8.0,
                width: 40.0,
                height: 30.0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        },
    );

    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![draw], vec![]))])
        .expect("capture clipped textured quad");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode clipped textured png");
    let bg = background_top_left(&img);
    let tolerance = 5;
    assert!(pixel_is_lit(&img, 24, 20, bg, tolerance), "pixel inside the textured clip rect should be painted");
    assert!(
        !pixel_is_lit(&img, 12, 20, bg, tolerance),
        "pixel inside the textured quad but outside the clip rect should remain clear",
    );
}

/// iamacoffeepot/aether#1777: a `capture_frame` carrying a `checks`
/// request returns a substrate-side verdict scored on the exact RGBA
/// the PNG is built from — no caller-side PNG decode. Draws a known
/// solid quad as a capture pre-mail and asserts the verdict's
/// reductions (`not_all_black`, `coverage`, `centroid`, `bounding_box`)
/// land the same way the decode-based `solid_quad_draws_screen_space_rect`
/// scores them, but computed in the render thread.
#[test]
#[allow(clippy::cast_precision_loss)]
// A single long end-to-end scenario (build → draw → capture → assert each
// reduction); splitting it would scatter the one linear story.
#[allow(clippy::too_many_lines)]
fn capture_frame_checks_return_substrate_verdict() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");

    // Known screen rect: top-left (16, 12), size 24×18 — the same draw
    // `solid_quad_draws_screen_space_rect` decodes the PNG to score.
    let (quad_x, quad_y, quad_w, quad_h) = (16.0f32, 12.0f32, 24.0f32, 18.0f32);
    let draw = envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![SolidQuad {
                x: quad_x,
                y: quad_y,
                width: quad_w,
                height: quad_h,
                color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        },
    );
    let tolerance = 5u8;
    let mk_check = |reduction| FrameCheck {
        reduction,
        tolerance,
        // None → partition against the frame's top-left pixel (the clear
        // color), matching the decode-based scenarios' convention.
        background: None,
        // None → score the whole frame; the region-scoped assertion below
        // (`capture_frame_region_scopes_reduction_to_one_widget_rect`)
        // demonstrates the composition target this whole-frame verdict
        // predates.
        region: None,
    };

    let result = harness
        .execute(vec![(
            "snap",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CaptureFrame {
                    window: None,
                    mails: vec![draw],
                    after_mails: vec![],
                    checks: vec![
                        mk_check(FrameReduction::NotAllBlack),
                        mk_check(FrameReduction::Coverage),
                        mk_check(FrameReduction::Centroid),
                        mk_check(FrameReduction::BoundingBox),
                    ],
                    similarity: None,
                },
            ),
        )])
        .expect("send_and_await_reply(CaptureFrame) with checks");
    let reply: CaptureFrameResult = result.reply("snap").expect("decode CaptureFrameResult");
    let verdict = match reply {
        CaptureFrameResult::Ok { png, verdict, .. } => {
            assert!(png.starts_with(&[0x89, 0x50, 0x4E, 0x47]), "the PNG still rides back alongside the verdict");
            verdict.expect("a checks request returns a verdict")
        }
        CaptureFrameResult::Err { error } => panic!("capture_frame replied Err: {error}"),
    };
    assert_eq!((verdict.width, verdict.height), (frame_width, frame_height));
    assert_eq!(verdict.results.len(), 4);

    match &verdict.results[0] {
        FrameCheckResult::NotAllBlack { passed, detail } => {
            assert!(passed, "the white quad lights pixels: {detail:?}");
        }
        other => panic!("expected NotAllBlack result, got {other:?}"),
    }
    match &verdict.results[1] {
        FrameCheckResult::Coverage { fraction, .. } => {
            // 24*18 / 64*48 ≈ 0.14 — the same band the decode test asserts.
            assert!((0.08..0.22).contains(fraction), "solid quad coverage {fraction} fell outside the expected band");
        }
        other => panic!("expected Coverage result, got {other:?}"),
    }
    match &verdict.results[2] {
        FrameCheckResult::Centroid { centroid, .. } => {
            let [cx, cy] = centroid.expect("a lit frame has a centroid");
            let pad = 4.0f32;
            assert!(
                cx >= quad_x - pad && cx <= quad_x + quad_w + pad && cy >= quad_y - pad && cy <= quad_y + quad_h + pad,
                "verdict centroid ({cx}, {cy}) should sit inside the screen rect",
            );
        }
        other => panic!("expected Centroid result, got {other:?}"),
    }
    match &verdict.results[3] {
        FrameCheckResult::BoundingBox { rect, .. } => {
            let rect = rect.expect("a lit frame has a bounding box");
            let pad = 4.0f32;
            let (min_x, max_x) = (rect.min_x as f32, rect.max_x as f32);
            assert!(
                min_x >= quad_x - pad
                    && min_x <= quad_x + pad
                    && max_x <= quad_x + quad_w + pad
                    && max_x >= quad_x + quad_w - pad,
                "verdict bounding box {rect:?} should hug the drawn rect's x-extent",
            );
        }
        other => panic!("expected BoundingBox result, got {other:?}"),
    }
}

/// Issue #2913 regression: a `CaptureFrame.similarity` request resolves
/// its reference image from the `SubstrateHarness`'s configured `assets`
/// namespace root, the same way the desktop chassis wires
/// `RenderConfig.assets_dir`. Captures a deterministic clear-color
/// frame, stores that exact PNG under the sandbox's assets root as the
/// reference, then requests a second capture with a `SimilarityCheck`
/// against it. Two captures of the same unchanged scene are pixel-
/// identical, so the score is `0.0` and the check passes — proving
/// `SubstrateHarnessChassis::build_passive` no longer leaves `assets_dir`
/// unconditionally `None` (the bug this issue fixes; on unfixed `main`
/// this fails at reference resolution with "no assets directory is
/// configured").
#[test]
fn capture_frame_similarity_resolves_reference_from_configured_assets_root() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-harness-render-similarity");
    let mut harness = SubstrateHarness::builder()
        .with_render()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let reference = harness.execute(vec![("reference", HarnessOp::capture())]).expect("capture reference frame");
    let reference_png = reference.captured("reference").expect("reference step ran");
    let reference_path = "similarity-reference.png";
    fs::write(sandbox.join(reference_path), reference_png).expect("write reference png under the sandbox assets root");

    let result = harness
        .execute(vec![(
            "snap",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CaptureFrame {
                    window: None,
                    mails: vec![],
                    after_mails: vec![],
                    checks: vec![],
                    similarity: Some(SimilarityCheck {
                        namespace: "assets".to_owned(),
                        reference_path: reference_path.to_owned(),
                        threshold: 0.0,
                    }),
                },
            ),
        )])
        .expect("send_and_await_reply(CaptureFrame) with similarity");
    let reply: CaptureFrameResult = result.reply("snap").expect("decode CaptureFrameResult");
    match reply {
        CaptureFrameResult::Ok { png, verdict, similarity_score, similarity_pass } => {
            assert!(
                png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
                "the PNG still rides back alongside the similarity score",
            );
            assert!(verdict.is_none(), "no checks were requested, so no intrinsic verdict should ride back");
            assert_eq!(similarity_score, Some(0.0), "an unchanged scene captured twice should score a perfect match");
            assert_eq!(similarity_pass, Some(true), "a 0.0 score against a 0.0 threshold must pass");
        }
        CaptureFrameResult::Err { error } => panic!(
            "capture_frame similarity replied Err (assets root not wired into SubstrateHarness?): \
             {error}"
        ),
    }
}

/// A region-scoped `FrameCheck` restricts a reduction to one screen
/// rect — the composition primitive a per-widget assertion needs so it
/// doesn't fold every widget in the scene into one whole-frame number
/// (iamacoffeepot/aether#2673). Draws two disjoint solid quads standing
/// in for two widgets and scores a region-scoped `coverage` +
/// `centroid` against only the first quad's rect: coverage lands near
/// 1.0 (the region is fully covered by its own quad, unlike the
/// whole-frame reading which would fold in the empty space between the
/// quads) and the centroid stays inside that quad rather than blending
/// toward the second quad the region excludes.
#[test]
fn capture_frame_region_scopes_reduction_to_one_widget_rect() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");

    let (first_x, first_y, first_w, first_h) = (4.0f32, 4.0f32, 12.0f32, 12.0f32);
    let (second_x, second_y, second_w, second_h) = (40.0f32, 4.0f32, 12.0f32, 12.0f32);
    let draw = envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![
                SolidQuad {
                    x: first_x,
                    y: first_y,
                    width: first_w,
                    height: first_h,
                    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                },
                SolidQuad {
                    x: second_x,
                    y: second_y,
                    width: second_w,
                    height: second_h,
                    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                },
            ],
        },
    );

    let tolerance = 5u8;
    // Region hugs the first quad's own screen rect exactly (pixel
    // coordinates matching first_x/first_y/first_w/first_h above),
    // leaving the second quad entirely outside it.
    let region = FrameRect { min_x: 4, min_y: 4, max_x: 15, max_y: 15 };
    let region_check = |reduction| FrameCheck { reduction, tolerance, background: None, region: Some(region) };

    let result = harness
        .execute(vec![(
            "snap",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CaptureFrame {
                    window: None,
                    mails: vec![draw],
                    after_mails: vec![],
                    checks: vec![region_check(FrameReduction::Coverage), region_check(FrameReduction::Centroid)],
                    similarity: None,
                },
            ),
        )])
        .expect("send_and_await_reply(CaptureFrame) with region-scoped checks");
    let reply: CaptureFrameResult = result.reply("snap").expect("decode CaptureFrameResult");
    let verdict = match reply {
        CaptureFrameResult::Ok { verdict, .. } => verdict.expect("a checks request returns a verdict"),
        CaptureFrameResult::Err { error } => panic!("capture_frame replied Err: {error}"),
    };
    assert_eq!(verdict.results.len(), 2);

    match &verdict.results[0] {
        FrameCheckResult::Coverage { fraction, .. } => {
            assert!(
                *fraction > 0.9,
                "region-scoped coverage {fraction} should be near 1.0 — the region is fully \
                 covered by its own quad",
            );
        }
        other => panic!("expected Coverage result, got {other:?}"),
    }
    match &verdict.results[1] {
        FrameCheckResult::Centroid { centroid, .. } => {
            let [cx, cy] = centroid.expect("the region has a lit centroid");
            assert!(
                cx >= first_x && cx <= first_x + first_w && cy >= first_y && cy <= first_y + first_h,
                "region-scoped centroid ({cx}, {cy}) should sit inside the first quad's rect, \
                 not blended toward the second quad the region excludes",
            );
        }
        other => panic!("expected Centroid result, got {other:?}"),
    }
}

/// `measurements.json`'s shape, mirrored locally so the scenario can
/// decode a persisted `ArtifactGuard` write and cross-check it against
/// the real `CaptureFrame` verdict it was armed from — `FrameCheck` /
/// `FrameCheckResult` already derive `Deserialize`, so only the
/// wrapping record needs restating.
#[derive(serde::Deserialize)]
struct PersistedMeasurements {
    id: String,
    checks: Vec<FrameCheck>,
    results: Vec<FrameCheckResult>,
}

/// Count of opaque-white pixels in a decoded mask, plus their mean and
/// bounding extent — the same three readings `diagnostic_mask`'s own
/// `run_checks`-agreement unit tests in `visual.rs` compute, just
/// derived here from the persisted PNG bytes rather than the in-memory
/// mask, since this scenario runs as a separate integration-test crate
/// with no access to `visual`'s crate-internal helper.
struct MaskStats {
    lit_count: usize,
    mean: Option<(f32, f32)>,
    bounds: Option<(u32, u32, u32, u32)>,
}

fn mask_stats(mask: &Image) -> MaskStats {
    let lit: Vec<(u32, u32)> = mask
        .rgba
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, pixel)| *pixel == [255, 255, 255, 255])
        .map(|(flat, _)| {
            #[allow(clippy::cast_possible_truncation)]
            let flat = flat as u32;
            (flat % mask.width, flat / mask.width)
        })
        .collect();
    if lit.is_empty() {
        return MaskStats { lit_count: 0, mean: None, bounds: None };
    }
    let (sum_x, sum_y) = lit.iter().fold((0u64, 0u64), |(sx, sy), &(x, y)| (sx + u64::from(x), sy + u64::from(y)));
    #[allow(clippy::cast_precision_loss)]
    let mean = (sum_x as f32 / lit.len() as f32, sum_y as f32 / lit.len() as f32);
    let bounds = lit.iter().fold((u32::MAX, 0u32, u32::MAX, 0u32), |(min_x, max_x, min_y, max_y), &(x, y)| {
        (min_x.min(x), max_x.max(x), min_y.min(y), max_y.max(y))
    });
    MaskStats { lit_count: lit.len(), mean: Some(mean), bounds: Some(bounds) }
}

/// Issue 2914: `ArtifactGuard` closes the failure-diagnostic gap for a
/// direct `SubstrateHarness` visual assertion. Draws the same known
/// solid-quad scene `capture_frame_checks_return_substrate_verdict`
/// scores, then exercises the guard's full write contract against the
/// real captured PNG and verdict:
///
/// - a panicking assertion persists `actual.png` (byte-identical to
///   the capture), `measurements.json`, and one `mask_N.png` per check
///   — each mask's own lit-pixel reading agreeing with the verdict
///   result it visualizes;
/// - an attached altered reference produces a deterministic
///   `difference.png`;
/// - a passing assertion (no panic) leaves no directory behind at all.
#[test]
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn artifact_guard_persists_actual_mask_and_measurements_on_panic() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut harness = SubstrateHarness::builder().size(frame_width, frame_height).with_render().build().expect("boot");

    let (quad_x, quad_y, quad_w, quad_h) = (16.0f32, 12.0f32, 24.0f32, 18.0f32);
    let draw = envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![SolidQuad {
                x: quad_x,
                y: quad_y,
                width: quad_w,
                height: quad_h,
                color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        },
    );
    let tolerance = 5u8;
    let mk_check = |reduction| FrameCheck { reduction, tolerance, background: None, region: None };
    let checks = vec![
        mk_check(FrameReduction::NotAllBlack),
        mk_check(FrameReduction::Coverage),
        mk_check(FrameReduction::Centroid),
        mk_check(FrameReduction::BoundingBox),
    ];

    let result = harness
        .execute(vec![(
            "snap",
            HarnessOp::send_and_await_reply(
                "aether.render",
                &CaptureFrame {
                    window: None,
                    mails: vec![draw],
                    after_mails: vec![],
                    checks: checks.clone(),
                    similarity: None,
                },
            ),
        )])
        .expect("send_and_await_reply(CaptureFrame) with checks");
    let reply: CaptureFrameResult = result.reply("snap").expect("decode CaptureFrameResult");
    let (png, verdict) = match reply {
        CaptureFrameResult::Ok { png, verdict, .. } => (png, verdict.expect("a checks request returns a verdict")),
        CaptureFrameResult::Err { error } => panic!("capture_frame replied Err: {error}"),
    };

    let panic_id = "artifact_guard_e2e_panic";
    let panic_dir = artifact_dir(panic_id);
    let _ = fs::remove_dir_all(&panic_dir);
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = ArtifactGuard::arm(panic_id, png.clone(), checks.clone(), verdict.results.clone());
        panic!("simulated failing visual assertion");
    }));
    assert!(outcome.is_err(), "test setup: the guarded closure must panic");

    let actual_bytes = fs::read(panic_dir.join("actual.png")).expect("actual.png should persist");
    assert_eq!(actual_bytes, png, "the persisted actual.png must be byte-identical to the exact captured PNG");

    let measurements_json =
        fs::read_to_string(panic_dir.join("measurements.json")).expect("measurements.json should persist");
    let measurements: PersistedMeasurements =
        serde_json::from_str(&measurements_json).expect("decode measurements.json");
    assert_eq!(measurements.id, panic_id);
    assert_eq!(measurements.checks, checks);
    assert_eq!(measurements.results, verdict.results);

    let total_pixels = f64::from(frame_width) * f64::from(frame_height);
    for (index, check_result) in verdict.results.iter().enumerate() {
        let mask_bytes = fs::read(panic_dir.join(format!("mask_{index}.png")))
            .unwrap_or_else(|error| panic!("mask_{index}.png should persist: {error}"));
        let mask = decode_png(&mask_bytes).expect("decode mask png");
        assert_eq!((mask.width, mask.height), (frame_width, frame_height));
        let stats = mask_stats(&mask);
        match check_result {
            FrameCheckResult::NotAllBlack { passed, .. } => {
                assert!(*passed, "test setup: the drawn quad must pass NotAllBlack");
                assert!(stats.lit_count > 0, "mask_{index} should show the lit quad");
            }
            FrameCheckResult::Coverage { fraction, .. } => {
                let mask_fraction = stats.lit_count as f64 / total_pixels;
                assert!(
                    (mask_fraction - f64::from(*fraction)).abs() < 0.02,
                    "mask_{index} lit fraction {mask_fraction} should agree with the verdict's \
                     coverage {fraction}",
                );
            }
            FrameCheckResult::Centroid { centroid, .. } => {
                let [cx, cy] = centroid.expect("a lit frame has a centroid");
                let (mean_x, mean_y) = stats.mean.expect("a lit mask should report a mean");
                assert!(
                    (mean_x - cx).abs() < 1.0 && (mean_y - cy).abs() < 1.0,
                    "mask_{index} mean ({mean_x}, {mean_y}) should agree with the verdict's \
                     centroid ({cx}, {cy})",
                );
            }
            FrameCheckResult::BoundingBox { rect, .. } => {
                let rect = rect.expect("a lit frame has a bounding box");
                let bounds = stats.bounds.expect("a lit mask should report bounds");
                assert_eq!(
                    bounds,
                    (rect.min_x, rect.max_x, rect.min_y, rect.max_y),
                    "mask_{index} bounding box should agree exactly with the verdict's",
                );
            }
            FrameCheckResult::DiffersFromBackground { .. } => {
                panic!("test setup: no DiffersFromBackground check was requested");
            }
        }
    }
    assert!(!panic_dir.join("reference.png").exists(), "no reference.png without an attached reference");
    assert!(!panic_dir.join("difference.png").exists(), "no difference.png without an attached reference");

    // An explicit altered reference produces a deterministic difference
    // image. The reference is all-black, so `difference.png`'s RGB
    // equals the actual capture's own RGB exactly.
    let reference_id = "artifact_guard_e2e_reference";
    let reference_dir = artifact_dir(reference_id);
    let _ = fs::remove_dir_all(&reference_dir);
    let all_black_reference =
        substrate_render::encode_png(&vec![0u8; (frame_width * frame_height * 4) as usize], frame_width, frame_height)
            .expect("encode all-black reference png");
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = ArtifactGuard::arm(reference_id, png.clone(), checks.clone(), verdict.results.clone())
            .with_reference_png(all_black_reference.clone());
        panic!("simulated failing similarity assertion");
    }));
    assert!(outcome.is_err(), "test setup: the guarded closure must panic");

    let reference_bytes = fs::read(reference_dir.join("reference.png")).expect("reference.png should persist");
    assert_eq!(reference_bytes, all_black_reference);
    let difference_bytes = fs::read(reference_dir.join("difference.png")).expect("difference.png should persist");
    let difference = decode_png(&difference_bytes).expect("decode difference png");
    let actual_image = decode_png(&png).expect("decode actual png");
    for (difference_pixel, actual_pixel) in difference.rgba.chunks_exact(4).zip(actual_image.rgba.chunks_exact(4)) {
        assert_eq!(&difference_pixel[..3], &actual_pixel[..3]);
        assert_eq!(difference_pixel[3], 255);
    }

    // A passing assertion (no panic) leaves no directory behind at all.
    let passing_id = "artifact_guard_e2e_passing";
    let passing_dir = artifact_dir(passing_id);
    let _ = fs::remove_dir_all(&passing_dir);
    {
        let _guard = ArtifactGuard::arm(passing_id, png.clone(), checks.clone(), verdict.results.clone());
        // Guard drops here without panicking.
    }
    assert!(!passing_dir.exists(), "a passing assertion must leave no artifact directory behind");

    let _ = fs::remove_dir_all(&panic_dir);
    let _ = fs::remove_dir_all(&reference_dir);
    let _ = fs::remove_dir_all(&passing_dir);
}
