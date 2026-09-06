//! `draw_shapes` pixel scenarios (ADR-0213): the rounded-box distance
//! field the overlay pass evaluates — radius, stroke, shadow, the circle
//! at full radius, and the per-batch scissor — each read back from an
//! in-process `SubstrateHarness` capture. Skipped when no wgpu adapter is
//! available; `AETHER_REQUIRE_RUNTIME=1` (CI) makes that skip a panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness
// knob, not cap config.
#![allow(clippy::disallowed_methods)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::visual::{Image, background_top_left, coverage, decode_png};
use aether_harness_substrate_capture::{
    RenderHarnessBuilderExt, RenderHarnessExt,
    test_helpers::{envelope, has_wgpu_adapter, pixel_is_lit, rgba_at},
};
use aether_kinds::{ClipRect, QuadSpace};
use aether_math::Rgba;
use aether_render::{DrawShapes, Shape, ShapeShadow, ShapeStroke};

/// Whether the scenario can run: a wgpu adapter is present, or the CI
/// gate demands one.
fn require_wgpu() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    assert!(env::var("AETHER_REQUIRE_RUNTIME").is_err(), "AETHER_REQUIRE_RUNTIME set but no wgpu adapter");
    eprintln!("skipping: no wgpu adapter");
    false
}

/// One white box at `(x, y)` of `width` × `height` with `corner_radius`,
/// no stroke and no shadow — the parts the individual scenarios add.
fn box_shape(x: f32, y: f32, width: f32, height: f32, corner_radius: f32) -> Shape {
    Shape { x, y, width, height, corner_radius, fill: Some(Rgba::WHITE), stroke: None, shadow: None }
}

/// Capture one `draw_shapes` batch in a `width` × `height` frame and
/// return the decoded frame.
fn capture_shapes(width: u32, height: u32, clip: Option<ClipRect>, shapes: Vec<Shape>) -> Image {
    let mut harness = SubstrateHarness::builder().size(width, height).with_render().build().expect("boot");
    let draw = envelope("aether.render", &DrawShapes { space: QuadSpace::Screen, clip, shapes });

    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(vec![draw], vec![]))]).expect("capture shapes");
    decode_png(captured.captured("snap").expect("snap step ran")).expect("decode shapes png")
}

const TOLERANCE: u8 = 5;

/// A rounded box lights its middle and its edge midpoints, leaves its
/// corner pixels clear, and stops at its edge: the distance field is
/// placed where the box says and the radius carves the corners a quad
/// could not.
#[test]
fn a_rounded_box_lights_its_edges_and_not_its_corners() {
    if !require_wgpu() {
        return;
    }
    // Box (16, 8) + 32×32, radius 12: the corner pixel (17, 9) is
    // 12 - sqrt(2) * 11 ≈ -3.6 px outside the arc; (18, 18) is inside.
    let img = capture_shapes(64, 48, None, vec![box_shape(16.0, 8.0, 32.0, 32.0, 12.0)]);
    let bg = background_top_left(&img);
    assert!(pixel_is_lit(&img, 32, 24, bg, TOLERANCE), "the centre of the box is filled");
    assert!(pixel_is_lit(&img, 32, 9, bg, TOLERANCE), "the top edge's midpoint is filled");
    assert!(pixel_is_lit(&img, 17, 24, bg, TOLERANCE), "the left edge's midpoint is filled");
    assert!(!pixel_is_lit(&img, 17, 9, bg, TOLERANCE), "the top-left corner pixel is carved off by the radius");
    assert!(!pixel_is_lit(&img, 46, 38, bg, TOLERANCE), "the bottom-right corner pixel is carved off by the radius");
    assert!(pixel_is_lit(&img, 20, 12, bg, TOLERANCE), "a pixel just inside the corner arc is filled");
    assert!(!pixel_is_lit(&img, 32, 6, bg, TOLERANCE), "a pixel above the box stays clear");
    assert!(!pixel_is_lit(&img, 50, 24, bg, TOLERANCE), "a pixel right of the box stays clear");

    // A 32×32 box with 12 px corners covers 1024 - (4 - π) * 144 ≈ 900
    // of 3072 pixels.
    let covered = coverage(&img, bg, TOLERANCE);
    assert!(
        (0.25..0.33).contains(&covered),
        "rounded box coverage {covered} fell outside the expected band (0.25, 0.33)",
    );
}

/// A radius at or above half the shorter side is a circle: the four
/// midpoints of a square box are lit and its four corners are not, and
/// the coverage is a quarter-pi of the box.
#[test]
fn a_full_radius_box_is_a_circle() {
    if !require_wgpu() {
        return;
    }
    let img = capture_shapes(64, 64, None, vec![box_shape(12.0, 12.0, 40.0, 40.0, 40.0)]);
    let bg = background_top_left(&img);
    for (x, y, edge) in [(32, 13, "top"), (32, 50, "bottom"), (13, 32, "left"), (50, 32, "right")] {
        assert!(pixel_is_lit(&img, x, y, bg, TOLERANCE), "the {edge} of the circle reaches the box's edge");
    }
    for (x, y, corner) in
        [(15, 15, "top-left"), (48, 15, "top-right"), (15, 48, "bottom-left"), (48, 48, "bottom-right")]
    {
        assert!(!pixel_is_lit(&img, x, y, bg, TOLERANCE), "the {corner} corner of the box is outside the circle");
    }

    // π * 20² ≈ 1257 of 4096 pixels.
    let covered = coverage(&img, bg, TOLERANCE);
    assert!((0.27..0.34).contains(&covered), "circle coverage {covered} fell outside the expected band (0.27, 0.34)");
}

/// A stroke with no fill is a ring: the band inside the edge is painted
/// and the middle is not, and the band is as wide as the stroke says.
#[test]
fn a_stroke_without_a_fill_is_a_ring() {
    if !require_wgpu() {
        return;
    }
    let ring = Shape {
        fill: None,
        stroke: Some(ShapeStroke { width_pixels: 4.0, color: Rgba::WHITE }),
        ..box_shape(8.0, 8.0, 48.0, 32.0, 0.0)
    };
    let img = capture_shapes(64, 48, None, vec![ring]);
    let bg = background_top_left(&img);
    assert!(pixel_is_lit(&img, 32, 9, bg, TOLERANCE), "the first pixel inside the top edge is in the band");
    assert!(pixel_is_lit(&img, 32, 11, bg, TOLERANCE), "the last pixel of the 4 px band is painted");
    assert!(!pixel_is_lit(&img, 32, 14, bg, TOLERANCE), "a pixel past the band is not");
    assert!(!pixel_is_lit(&img, 32, 24, bg, TOLERANCE), "the middle of a ring is clear");
    assert!(pixel_is_lit(&img, 9, 24, bg, TOLERANCE), "the band runs down the left edge too");
    assert!(!pixel_is_lit(&img, 32, 6, bg, TOLERANCE), "the stroke lies inside the edge, not outside it");
}

/// A shadow falls outside the fill on the side its offset pushes it to,
/// fades with distance, and does not reach the side it pulls away from.
#[test]
fn a_shadow_falls_where_its_offset_sends_it_and_fades() {
    if !require_wgpu() {
        return;
    }
    // Box (16, 8) + 32×24 with an 8 px blur pushed 6 px down: the shadow
    // reaches 14 px below the bottom edge (y = 32) and 2 px above the top.
    let shadowed = Shape {
        shadow: Some(ShapeShadow { blur_pixels: 8.0, offset: [0.0, 6.0], color: Rgba::WHITE }),
        ..box_shape(16.0, 8.0, 32.0, 24.0, 0.0)
    };
    let img = capture_shapes(64, 64, None, vec![shadowed]);
    let bg = background_top_left(&img);
    let brightness = |y: u32| u32::from(rgba_at(&img, 32, y)[0]);
    assert!(pixel_is_lit(&img, 32, 34, bg, TOLERANCE), "the shadow is seen just below the bottom edge");
    assert!(brightness(34) > brightness(38), "the shadow fades: 2 px below the edge is brighter than 6 px below");
    assert!(brightness(38) > brightness(42), "…and 6 px below is brighter than 10 px below");
    assert!(!pixel_is_lit(&img, 32, 50, bg, TOLERANCE), "well past the blur the frame is clear");
    assert!(
        !pixel_is_lit(&img, 32, 4, bg, TOLERANCE),
        "above the box, where the offset pulled the shadow from, is clear"
    );
    assert!(pixel_is_lit(&img, 32, 20, bg, TOLERANCE), "the fill itself is untouched");
}

/// A shape batch carries the same per-batch framebuffer scissor the quad
/// batches do: pixels of the shape outside the clip stay clear, and a
/// following unclipped batch draws outside it again.
#[test]
fn a_shape_batch_is_bounded_by_its_clip() {
    if !require_wgpu() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");
    let clipped = envelope(
        "aether.render",
        &DrawShapes {
            space: QuadSpace::Screen,
            clip: Some(ClipRect { x: 20.0, y: 12.0, width: 12.0, height: 10.0 }),
            shapes: vec![box_shape(10.0, 8.0, 44.0, 30.0, 4.0)],
        },
    );
    let unclipped = envelope(
        "aether.render",
        &DrawShapes { space: QuadSpace::Screen, clip: None, shapes: vec![box_shape(44.0, 30.0, 8.0, 8.0, 0.0)] },
    );

    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![clipped, unclipped], vec![]))])
        .expect("capture clipped shapes");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode clipped shapes png");
    let bg = background_top_left(&img);
    assert!(pixel_is_lit(&img, 24, 16, bg, TOLERANCE), "a pixel inside the clip is painted");
    assert!(!pixel_is_lit(&img, 16, 16, bg, TOLERANCE), "a pixel of the shape outside the clip stays clear");
    assert!(pixel_is_lit(&img, 48, 34, bg, TOLERANCE), "the following unclipped batch paints outside the clip");

    // The harness's shape view of the committed overlay reports both
    // batches in submission order with their clips — what a widget
    // scenario reads instead of the pixels.
    let snapshot = harness.committed_shape_snapshot();
    assert_eq!(snapshot.len(), 2, "both shape batches were recorded; snapshot: {snapshot:?}");
    assert_eq!(snapshot[0].clip, Some(ClipRect { x: 20.0, y: 12.0, width: 12.0, height: 10.0 }));
    assert_eq!(snapshot[0].shapes[0].corner_radius, 4.0);
    assert_eq!(snapshot[1].clip, None);
    assert_eq!(snapshot[1].shapes[0].x, 44.0);
}
