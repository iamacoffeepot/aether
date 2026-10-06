//! Text scenarios (ADR-0105, ADR-0248 §10): the renderer registers a font
//! from a blob, reports its metrics, and draws strings as one overlay
//! batch in its sender's send order, each read back from an in-process
//! `SubstrateHarness` capture. Every harness composes the renderer alone:
//! there is no text actor and no `aether.fs`.
//!
//! The font is the workspace's vendored Roboto Mono (SIL OFL 1.1) at
//! `crates/aether-render/assets/fonts/RobotoMono.ttf`, embedded here and
//! handed to `create_font` as a blob.
//!
//! Skipped when no wgpu adapter is available; `AETHER_REQUIRE_RUNTIME=1`
//! (CI) makes that skip a panic.

use aether_data::Blob;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::visual::{
    Image, background_top_left, bounding_box, centroid, coverage, decode_png,
};
use aether_harness_substrate_capture::{
    RenderHarnessBuilderExt,
    test_helpers::{envelope, pixel_is_lit, require_wgpu_adapter},
};
use aether_kinds::{CachedFontMetrics, ClipRect, NamedMail, QuadScale, QuadSpace};
use aether_math::{Mat4, Rgba, Vec3};
use aether_render::{
    CreateFont, CreateFontResult, DrawShapes, DrawText, FontMetricsRequest, FontMetricsResult, RenderCapability, Shape,
    TextRun, ViewProjection, ViewportExtent,
};

/// The vendored font file, whole.
const FONT: &[u8] = include_bytes!("../assets/fonts/RobotoMono.ttf");

const TOLERANCE: u8 = 5;

/// A harness of `width` × `height` composing the renderer and nothing else.
fn render_harness(width: u32, height: u32) -> SubstrateHarness {
    SubstrateHarness::builder().with_render().size(width, height).build().expect("boot")
}

/// Ask the renderer to register `bytes` as a font and return its reply.
fn create_font_reply(harness: &mut SubstrateHarness, bytes: &[u8]) -> CreateFontResult {
    let create = CreateFont { bytes: Blob::from(bytes.to_vec()) };
    let render = harness.actor_ref::<RenderCapability>();

    harness
        .execute(vec![("create", HarnessOp::send_and_await_reply(&render, &create))])
        .expect("create_font sequence")
        .reply::<CreateFontResult>("create")
        .expect("decode CreateFontResult")
}

/// Register the vendored font and return its `font_id`.
fn create_font(harness: &mut SubstrateHarness) -> u32 {
    match create_font_reply(harness, FONT) {
        CreateFontResult::Ok { font_id } => font_id,
        CreateFontResult::Err { error } => panic!("create_font failed: {error}"),
    }
}

/// One white run of `text` at `size_pixels` from `origin`.
fn run(font_id: u32, text: &str, size_pixels: f32, origin: [f32; 2]) -> TextRun {
    TextRun { font_id, text: text.to_owned(), size_pixels, color: Rgba::WHITE, origin }
}

/// A `draw_text` of `runs` in screen space under `clip`, addressed to the
/// renderer.
fn screen_text(clip: Option<ClipRect>, runs: Vec<TextRun>) -> NamedMail {
    envelope("aether.render", &DrawText { clip, space: QuadSpace::Screen, runs })
}

/// Capture one frame whose pre-mails are `pre`, in that order.
fn capture(harness: &mut SubstrateHarness, pre: Vec<NamedMail>) -> Image {
    let captured = harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture");
    decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png")
}

fn lit_fraction_in_rect(img: &Image, x: u32, y: u32, width: u32, height: u32, bg: [u8; 3]) -> f32 {
    let mut lit = 0u32;
    for py in y..y + height {
        for px in x..x + width {
            if pixel_is_lit(img, px, py, bg, TOLERANCE) {
                lit += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        lit as f32 / (width * height) as f32
    }
}

/// Catches a first text draw that shows nothing (the atlas texture not
/// registered in the same call, or a new glyph's pixels not staged before
/// the frame that samples them), and a staged font parse whose completion
/// never reaches the pumped render slot, which would leave `create_font`
/// unanswered. One `draw_text`, the first of the session, is the only
/// thing that can light a pixel.
#[test]
#[allow(clippy::cast_precision_loss)]
fn the_first_draw_of_a_created_font_shows_its_glyphs() {
    if !require_wgpu_adapter() {
        return;
    }
    let (frame_width, frame_height) = (128u32, 64u32);
    let mut harness = render_harness(frame_width, frame_height);
    let font_id = create_font(&mut harness);

    let img = capture(&mut harness, vec![screen_text(None, vec![run(font_id, "Hi", 32.0, [0.0, 0.0])])]);

    // Sparse but present: rules out an empty frame and a full-bleed one.
    let bg = background_top_left(&img);
    let drawn = coverage(&img, bg, TOLERANCE);
    assert!((0.005..0.40).contains(&drawn), "text coverage {drawn} is outside (0.005, 0.40)");
    // Text flowing from the top-left origin lands in the upper-left.
    let (center_x, center_y) = centroid(&img, bg, TOLERANCE).expect("a lit frame has a centroid");
    assert!(center_y < frame_height as f32 / 2.0, "text centroid y={center_y} sits in the top half");
    assert!(center_x < frame_width as f32 * 0.75, "text centroid x={center_x} sits toward the left");
}

/// Catches a text batch filed out of its sender's send order: both draws
/// go to the renderer through one queue, so the one sent second lies over
/// the one sent first. An opaque box past every edge of the frame, sent
/// after the text, hides every glyph; sent before it, the glyphs show on
/// the box.
#[test]
fn a_shape_sent_after_text_covers_it_and_one_sent_before_does_not() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = render_harness(128, 64);
    let font_id = create_font(&mut harness);
    let text = || screen_text(None, vec![run(font_id, "MMMM", 32.0, [8.0, 8.0])]);
    let plate = || {
        let shape = Shape {
            x: -8.0,
            y: -8.0,
            width: 144.0,
            height: 80.0,
            corner_radius: 0.0,
            fill: Some(Rgba::new(0.1, 0.3, 0.8, 1.0)),
            stroke: None,
            shadow: None,
            texture: None,
        };

        envelope("aether.render", &DrawShapes { space: QuadSpace::Screen, clip: None, shapes: vec![shape] })
    };

    let covered = capture(&mut harness, vec![text(), plate()]);
    let over = capture(&mut harness, vec![plate(), text()]);

    let covered_glyphs = coverage(&covered, background_top_left(&covered), TOLERANCE);
    assert_eq!(covered_glyphs, 0.0, "a plate sent after the text hides every glyph pixel");
    let over_glyphs = coverage(&over, background_top_left(&over), TOLERANCE);
    assert!(over_glyphs > 0.005, "text sent after the plate shows on it; coverage={over_glyphs}");
}

/// Issue #2855. Catches a text batch that drops its clip on the way to
/// the overlay pass: glyph pixels outside the clip stay background, and
/// the same region is lit when the batch carries no clip.
#[test]
fn a_text_clip_bounds_its_glyph_pixels() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = render_harness(128, 64);
    let font_id = create_font(&mut harness);
    let draw = |clip| screen_text(clip, vec![run(font_id, "MMMMMMMM", 32.0, [8.0, 8.0])]);

    let unclipped = capture(&mut harness, vec![draw(None)]);
    let clipped = capture(&mut harness, vec![draw(Some(ClipRect { x: 18.0, y: 12.0, width: 22.0, height: 24.0 }))]);

    let unclipped_outside = lit_fraction_in_rect(&unclipped, 58, 18, 18, 18, background_top_left(&unclipped));
    assert!(unclipped_outside > 0.05, "unclipped text lights the sampled region; coverage={unclipped_outside}");
    let bg = background_top_left(&clipped);
    let inside = lit_fraction_in_rect(&clipped, 20, 18, 14, 14, bg);
    assert!(inside > 0.05, "clipped text still lights pixels inside the clip; coverage={inside}");
    let outside = lit_fraction_in_rect(&clipped, 58, 18, 18, 18, bg);
    assert_eq!(outside, 0.0, "glyph pixels outside the clip stay background");
}

/// Issue 1773. Catches a screen-space run that ignores its `origin`: the
/// lit centroid moves right and down by at least half the offset applied.
#[test]
#[allow(clippy::cast_precision_loss)]
fn a_screen_origin_shifts_the_text_centroid() {
    if !require_wgpu_adapter() {
        return;
    }
    let (frame_width, frame_height) = (256u32, 128u32);
    let mut harness = render_harness(frame_width, frame_height);
    let font_id = create_font(&mut harness);
    let (offset_x, offset_y) = ((frame_width / 2) as f32, (frame_height / 2) as f32);

    let at_zero = capture(&mut harness, vec![screen_text(None, vec![run(font_id, "Hi", 24.0, [0.0, 0.0])])]);
    let shifted = capture(&mut harness, vec![screen_text(None, vec![run(font_id, "Hi", 24.0, [offset_x, offset_y])])]);

    let bg = background_top_left(&at_zero);
    let base = centroid(&at_zero, bg, TOLERANCE).expect("the zero-origin frame has lit pixels");
    let moved = centroid(&shifted, bg, TOLERANCE).expect("the offset-origin frame has lit pixels");
    assert!(moved.0 > base.0 + offset_x / 2.0, "centroid x {} is right of {} by half of {offset_x}", moved.0, base.0);
    assert!(moved.1 > base.1 + offset_y / 2.0, "centroid y {} is below {} by half of {offset_y}", moved.1, base.1);
}

/// ADR-0105 font-metrics grab (issue 1854). Catches a metrics table whose
/// advances do not reproduce the draw path's: a string measured locally
/// from the cached table has exactly the extent fontdue's pen walk gives
/// it, which is how `draw_text` lays it out.
#[test]
fn the_font_metrics_table_measures_a_string_as_the_draw_path_lays_it_out() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = render_harness(64, 32);
    let font_id = create_font(&mut harness);
    let render = harness.actor_ref::<RenderCapability>();

    let grabbed = harness
        .execute(vec![("grab", HarnessOp::send_and_await_reply(&render, &FontMetricsRequest { font_id }))])
        .expect("font_metrics sequence");
    let metrics = match grabbed.reply::<FontMetricsResult>("grab").expect("decode FontMetricsResult") {
        FontMetricsResult::Ok { metrics } => metrics,
        FontMetricsResult::Err { error } => panic!("font_metrics failed: {error}"),
    };

    let (text, size) = ("Hello aether", 29.0);
    let local = CachedFontMetrics::new(&metrics).measure(text, size);
    let font = fontdue::Font::from_bytes(FONT, fontdue::FontSettings::default()).expect("vendored Roboto Mono parses");
    let mut draw_pen = 0.0f32;
    for ch in text.chars() {
        draw_pen += font.metrics(ch, size).advance_width;
    }
    assert!(local > 0.0, "a non-empty run has positive extent");
    assert_eq!(local, draw_pen, "the local measure equals the draw path's advance sum exactly");
}

/// Catches a `create_font` whose held reply is never answered on the
/// failure path, bytes that register a font anyway, and a metrics grab
/// that answers an unknown id with some other font.
#[test]
fn malformed_font_bytes_and_an_unknown_font_id_reply_err() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = render_harness(64, 32);
    let render = harness.actor_ref::<RenderCapability>();

    let created = create_font_reply(&mut harness, &[0xDE, 0xAD, 0xBE, 0xEF]);
    let grabbed = harness
        .execute(vec![("grab", HarnessOp::send_and_await_reply(&render, &FontMetricsRequest { font_id: 99 }))])
        .expect("font_metrics sequence");

    let CreateFontResult::Err { error } = created else {
        panic!("malformed bytes do not create a font: {created:?}");
    };
    assert!(error.contains("parse"), "the reply is the parse error: {error}");
    let FontMetricsResult::Err { error } = grabbed.reply::<FontMetricsResult>("grab").expect("decode") else {
        panic!("an unknown font id is refused");
    };
    assert!(error.contains("99"), "the refusal names the id: {error}");
}

/// The lit bounding box's width, in pixels, of one capture whose pre-mails
/// are `view` then a world-space `draw_text` of `label`.
#[allow(clippy::cast_precision_loss)]
fn world_label_width(harness: &mut SubstrateHarness, view: &ViewProjection, label: &DrawText) -> f32 {
    let img = capture(harness, vec![envelope("aether.render", view), envelope("aether.render", label)]);
    let lit = bounding_box(&img, background_top_left(&img), TOLERANCE).expect("the label lights the frame");

    (lit.max_x - lit.min_x + 1) as f32
}

/// ADR-0105 world-space text (issue 1699). Catches a world label drawn
/// away from its anchor's projection or under the wrong scale: a
/// `Distance` label halves in width when the camera doubles its distance,
/// a `Pixels` label holds its width, and a `Pixels` label keeps its width
/// under a 45-degree orbit, so it faces the camera and is not skewed.
#[test]
#[allow(clippy::cast_precision_loss)]
fn a_world_space_label_draws_at_its_anchor() {
    use std::f32::consts::PI;

    if !require_wgpu_adapter() {
        return;
    }
    let (frame_width, frame_height) = (128u32, 96u32);
    let mut harness = render_harness(frame_width, frame_height);
    let font_id = create_font(&mut harness);
    let projection = Mat4::perspective_rh(PI / 3.0, frame_width as f32 / frame_height as f32, 0.1, 100.0);
    let view_from = |eye: Vec3| ViewProjection {
        view: Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::new(0.0, 1.0, 0.0)),
        projection,
        eye,
        near: 0.1,
        far: 100.0,
        extent: ViewportExtent { width: frame_width, height: frame_height },
    };
    let near = view_from(Vec3::new(0.0, 0.0, 10.0));
    let far = view_from(Vec3::new(0.0, 0.0, 20.0));
    let orbit = view_from(Vec3::new(10.0 * (PI / 4.0).sin(), 0.0, 10.0 * (PI / 4.0).cos()));
    let label = |scale: QuadScale| DrawText {
        clip: None,
        space: QuadSpace::World { anchor: [0.0, 0.0, 0.0], scale },
        runs: vec![run(font_id, "Hy", 24.0, [0.0, 0.0])],
    };
    let by_distance = label(QuadScale::Distance { reference_distance: 10.0 });
    let by_pixels = label(QuadScale::Pixels);

    let distance_ratio =
        world_label_width(&mut harness, &far, &by_distance) / world_label_width(&mut harness, &near, &by_distance);
    let pixels_near = world_label_width(&mut harness, &near, &by_pixels);
    let pixels_ratio = world_label_width(&mut harness, &far, &by_pixels) / pixels_near;
    let orbit_ratio = world_label_width(&mut harness, &orbit, &by_pixels) / pixels_near;

    // Pixel-grid rounding moves each ratio a little; the bands are wide
    // enough for that and narrow enough to tell the three modes apart.
    assert!((0.25..0.75).contains(&distance_ratio), "Distance width at d=20 over d=10 is {distance_ratio:.3}, not 0.5");
    assert!((0.80..1.25).contains(&pixels_ratio), "Pixels width at d=20 over d=10 is {pixels_ratio:.3}, not 1.0");
    assert!((0.70..1.43).contains(&orbit_ratio), "Pixels width under a 45-degree orbit is {orbit_ratio:.3}, not 1.0");
}
