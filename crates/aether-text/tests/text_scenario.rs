//! Text-cap harness scenarios (rehomed from `aether-substrate-bundle`'s
//! `substrate_harness_scenario/text.rs`, issue #3772): `aether.text.draw`
//! screen-space glyphs, per-draw clip bounds, the font-metrics grab, and
//! world-space labels, each driven through an in-process
//! `SubstrateHarness`.
//!
//! Every harness composes exactly the caps its scenario needs (issue
//! #3764): the render cap via `RenderHarnessBuilderExt::with_render` (the
//! text cap composes render's texture / quad surface by mail and the
//! assertions read captured frames), the text cap via
//! `.with_actor::<TextCapability>(())`, and the `aether.fs` cap via
//! `.namespace_roots(...)` so `aether.text.load_font` can read the TTF
//! through the `assets` namespace.
//!
//! The font is the workspace's vendored Roboto Mono (SIL OFL 1.1) at
//! `crates/aether-text/assets/fonts/RobotoMono.ttf` — the
//! `assets` namespace root points straight at that in-repo home (the
//! same file `aether-text`'s runtime unit tests and the bundle's widget
//! scenarios read) rather than copying the binary next to this test.
//!
//! Skipped when no wgpu adapter is available (driverless Linux runners
//! without `mesa-vulkan-drivers`); `AETHER_REQUIRE_RUNTIME=1` (CI sets
//! it) flips the skip into a hard panic so a CI-side regression is loud.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness
// knob, not cap config.
#![allow(clippy::disallowed_methods)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use aether_fs::NamespaceRoots;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::visual::{
    Image, background_top_left, bounding_box, centroid, coverage, decode_png,
};
use aether_harness_substrate_capture::{
    RenderHarnessBuilderExt,
    test_helpers::{envelope, has_wgpu_adapter, init_save_sandbox, pixel_is_lit},
};
use aether_kinds::{CachedFontMetrics, ClipRect, QuadScale, QuadSpace};
use aether_math::{Mat4, Rgba, Vec3};
use aether_render::{DrawSolidQuads, SolidQuad, ViewProjection};
use aether_text::{DrawText, FontMetricsRequest, FontMetricsResult, FontRef, LoadFont, LoadFontResult, TextCapability};

/// Namespace-relative path of the vendored font under the `assets` root.
const FONT_PATH: &str = "fonts/RobotoMono.ttf";

/// The workspace's shared font-asset home: this crate's own `assets`
/// dir (which holds `fonts/RobotoMono.ttf` — rehomed from the chassis
/// bundle at the by-chassis split, #3814).
fn font_assets_root() -> PathBuf {
    match env::current_dir().map(|current| current.join("assets")) {
        Ok(dir) if dir.is_dir() => dir,
        _ => Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"),
    }
}

/// `NamespaceRoots` for these scenarios: `assets` points at the in-repo
/// font home (read-only use), while `save` / `config` land in the
/// per-process sandbox so nothing writable escapes the test.
fn font_namespace_roots() -> NamespaceRoots {
    let sandbox = init_save_sandbox("text-scenario");
    NamespaceRoots { save: sandbox.to_path_buf(), assets: font_assets_root(), config: sandbox.to_path_buf() }
}

fn lit_fraction_in_rect(img: &Image, x: u32, y: u32, width: u32, height: u32, bg: [u8; 3], tolerance: u8) -> f32 {
    let mut lit = 0u32;
    for py in y..y + height {
        for px in x..x + width {
            if pixel_is_lit(img, px, py, bg, tolerance) {
                lit += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        lit as f32 / (width * height) as f32
    }
}

/// The scenarios here need wgpu (the composed render cap builds a `Gpu`
/// at boot) but no wasm. Skips on wgpu-less runners and panics under
/// `AETHER_REQUIRE_RUNTIME` so a CI-side regression is loud.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// ADR-0105 text surface end to end: load a real OFL TTF through the
/// `assets` namespace, draw a `Screen`-space string, and assert the
/// captured frame lights a region in the upper-left where top-left-
/// anchored text lands. No component is loaded — the text is the only
/// thing that can light a pixel.
///
/// The first `draw` lazily creates the atlas texture (and draws nothing
/// that turn); `send_and_await_reply` settles that `create_texture` round trip,
/// so the texture id is live before the capture's pre-mail `draw`
/// rasterizes glyphs and emits the quad batch.
#[test]
#[allow(clippy::cast_precision_loss)]
fn text_draws_a_screen_space_string() {
    if !require_wgpu_only() {
        return;
    }

    let (frame_width, frame_height) = (128u32, 64u32);
    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_actor::<TextCapability>(())
        .size(frame_width, frame_height)
        .namespace_roots(font_namespace_roots())
        .build()
        .expect("boot");

    // Load the font; the reply carries the session-scoped font_id.
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: FONT_PATH.to_owned() },
            ),
        )])
        .expect("load_font sequence");
    let font_id = match loaded.reply::<LoadFontResult>("load").expect("decode LoadFontResult") {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load_font failed: {error}"),
    };

    let draw = DrawText {
        font_id,
        text: "Hi".to_owned(),
        size_pixels: 32.0,
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
        origin: [0.0, 0.0],
        space: QuadSpace::Screen,
        clip: None,
    };

    // First draw: lazily creates the atlas texture (fire-and-forget — a
    // `draw` has no reply). The advance pumps the `create_texture` reply
    // back into the text cap so its texture id is live; nothing is drawn
    // this turn.
    harness
        .execute(vec![
            ("prime", HarnessOp::send_and_settle::<DrawText>("aether.text", &draw)),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("prime draw");

    // Now the glyphs rasterize and the quad batch reaches the renderer the
    // same tick the capture records.
    let pre = vec![envelope("aether.text", &draw)];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // Sparse but present — rules out an empty frame and a full-bleed one.
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.005..0.40).contains(&drawn),
        "text coverage {drawn} fell outside the expected band (0.005, 0.40); \
         the captured frame is effectively empty or entirely filled",
    );

    // Top-left-anchored text lands in the upper-left: the lit centroid
    // sits in the top half and left portion of the frame.
    let (center_x, center_y) = centroid(&img, bg, tolerance).expect("a lit frame has a centroid");
    assert!(
        center_y < frame_height as f32 / 2.0,
        "text centroid y={center_y} should sit in the top half (anchored at the top edge)",
    );
    assert!(
        center_x < frame_width as f32 * 0.75,
        "text centroid x={center_x} should sit toward the left (anchored at the left edge)",
    );

    // The lit box must not bleed to the far-right / bottom edges — the
    // short string occupies only the upper-left.
    let silhouette = bounding_box(&img, bg, tolerance).expect("a lit frame has a bounding box");
    assert!(
        silhouette.min_x < frame_width / 2 && silhouette.max_y < frame_height,
        "text silhouette {silhouette:?} should bound the upper-left of the \
         {frame_width}x{frame_height} frame",
    );
}

/// ADR-0161 R4 regression (issue #3917, the redo of the reverted #3923): a
/// harness capture must pin the **current** tick's rendered content, never a
/// stale prior frame. The reverted slice drove the capture frame while the
/// capture was merely *pending* — before every pre-mail chain had drained its
/// draw onto the render accumulator — so a frame recorded mid-fill: it
/// committed the batches that had landed, cleared the accumulator, and the
/// later-arriving batches from the same capture landed in the *next* frame,
/// dropping the earlier ones. The fix drives exactly one frame, and only once
/// the capture is *ready* (every pre-mail chain settled with the slot drained),
/// so a single commit records the whole draw set.
///
/// The reproduction is the panel's shape, minimized to a producer whose output
/// changes every tick: two overlay batches that land in *different* pump drains
/// but share the one `quad_frame` accumulator, so a mid-fill commit drops one
/// of them. The first is a **direct** `DrawSolidQuads`, dispatched by
/// `on_capture_frame` straight to `aether.render`, so it lands on the first
/// drain; the second is a `DrawText` through `aether.text`, whose cap lays the
/// string out on its own thread and emits the glyph `draw_textured_quads` a hop
/// later, so it lands on a *later* drain.
///
/// The solid quad moves to a fresh column each iteration ("output changes every
/// tick"); the capture must show it at its **current** column. Under the
/// reverted ordering the first-committed batch (the solid quad) is the one the
/// mid-fill frame drops, so its region reads background — the stale-frame
/// signature. Because the drop is racy (the two batches sometimes coincide on
/// one drain), issue #3917 runs this in a ≥20-iteration loop alongside the two
/// widget scenarios; the per-capture loop below also multiplies the chances.
///
/// Skips without wgpu like the sibling scenarios; `AETHER_REQUIRE_RUNTIME`
/// (CI) makes the skip a hard failure.
///
// Tripwire: under the reverted capture-versus-drain ordering the moving solid
// quad's current column reads background (its batch was dropped by a mid-fill
// commit), firing the current-column-lit assertion.
#[test]
#[allow(clippy::cast_precision_loss)]
fn capture_pins_current_tick_content_not_stale_frame() {
    if !require_wgpu_only() {
        return;
    }

    let (frame_width, frame_height) = (128u32, 64u32);
    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_actor::<TextCapability>(())
        .size(frame_width, frame_height)
        .namespace_roots(font_namespace_roots())
        .build()
        .expect("boot");

    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: FONT_PATH.to_owned() },
            ),
        )])
        .expect("load_font sequence");
    let font_id = match loaded.reply::<LoadFontResult>("load").expect("decode LoadFontResult") {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load_font failed: {error}"),
    };

    // Prime the atlas: the first-ever draw of the session lazily creates the
    // atlas texture (an async `create_texture` round trip through the renderer)
    // and draws nothing that turn. Settling it now means the per-iteration draw
    // only has to rasterize + lay out its glyphs and emit the quad batch — the
    // per-iteration strings below use fresh, uncached glyphs so that emit lands
    // a drain *after* the direct solid quad, the split the race needs.
    let prime_draw = DrawText {
        font_id,
        text: "prime".to_owned(),
        size_pixels: 28.0,
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
        origin: [0.0, 0.0],
        space: QuadSpace::Screen,
        clip: None,
    };
    harness
        .execute(vec![
            ("prime", HarnessOp::send_and_settle::<DrawText>("aether.text", &prime_draw)),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("prime atlas");

    // A 16×16 solid quad on the bottom row, stepped across four columns so
    // each capture's content differs from the last. Positions stay in integer
    // window pixels for the region reads; only the `SolidQuad`'s float fields
    // widen to `f32` (the `cast_precision_loss` these small ints incur is
    // covered by the fn-level allow, matching the sibling scenarios).
    let quad_size = 16u32;
    let quad_y = frame_height - quad_size - 4;
    let columns: [u32; 4] = [8, 40, 72, 104];
    // A distinct, previously-unseen glyph string each iteration forces the text
    // cap to rasterize fresh glyphs (not replay a cached batch), so its
    // draw_textured_quads emit reliably trails the direct solid quad by a
    // drain — heavier work than a cached string, matching the widget panel's
    // multi-batch fan-out that made the reverted race observable.
    let strings = ["alpha", "bravo", "charlie", "delta"];
    let tolerance = 5u8;

    let mut prior_x: Option<u32> = None;
    for (i, &quad_x) in columns.iter().enumerate() {
        let solid = DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![SolidQuad {
                x: quad_x as f32,
                y: quad_y as f32,
                width: quad_size as f32,
                height: quad_size as f32,
                color: Rgba::new(0.9, 0.9, 0.2, 1.0),
            }],
        };
        let glyph_draw = DrawText {
            font_id,
            text: strings[i].to_owned(),
            size_pixels: 28.0,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            origin: [0.0, 0.0],
            space: QuadSpace::Screen,
            clip: None,
        };
        // Order matters: the direct solid quad is dispatched first, so it lands
        // on the first drain and is the batch a mid-fill commit would strand.
        let pre = vec![envelope("aether.render", &solid), envelope("aether.text", &glyph_draw)];
        let label = format!("frame_{i}");
        let captured = harness
            .execute(vec![(label.as_str(), HarnessOp::capture_with_mails(pre, vec![]))])
            .expect("capture-with-mails");
        let img = decode_png(captured.captured(&label).expect("capture step ran")).expect("decode capture png");
        let bg = background_top_left(&img);

        // The glyphs (drain-2 batch) light the upper-left every frame — a
        // sanity floor that the capture is not simply empty.
        let glyph_lit = lit_fraction_in_rect(&img, 0, 0, 40, 32, bg, tolerance);
        assert!(glyph_lit > 0.02, "frame {i}: the glyph batch is missing (lit {glyph_lit}) — capture drew nothing");

        // The current tick's solid quad must be present at its column. Under
        // the reverted ordering this drain-1 batch is stranded by the mid-fill
        // commit, so its region reads background.
        let current = lit_fraction_in_rect(&img, quad_x, quad_y, quad_size, quad_size, bg, tolerance);
        assert!(
            current > 0.8,
            "frame {i}: the current tick's solid quad at column x={quad_x} is missing (lit {current}) — \
             the capture recorded a stale frame that dropped the drain-1 batch",
        );

        // And the prior tick's column must have cleared — the capture reflects
        // the latest tick's placement, not an accreted or replayed prior frame.
        if let Some(prior_x) = prior_x {
            let stale = lit_fraction_in_rect(&img, prior_x, quad_y, quad_size, quad_size, bg, tolerance);
            assert!(
                stale < 0.2,
                "frame {i}: the prior tick's solid quad at column x={prior_x} is still lit (lit {stale}) — \
                 the capture shows stale content",
            );
        }
        prior_x = Some(quad_x);
    }
}

/// Issue #2855: `aether.text.draw` forwards its framebuffer clip to the
/// textured glyph quad batch it emits, so glyph pixels outside the clip
/// are discarded by the same overlay-pass scissor as primitive quads.
#[test]
#[allow(clippy::too_many_lines)]
fn text_draw_clip_bounds_glyph_pixels() {
    if !require_wgpu_only() {
        return;
    }

    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_actor::<TextCapability>(())
        .size(128, 64)
        .namespace_roots(font_namespace_roots())
        .build()
        .expect("boot");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: FONT_PATH.to_owned() },
            ),
        )])
        .expect("load_font sequence");
    let font_id = match loaded.reply::<LoadFontResult>("load").expect("decode LoadFontResult") {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load_font failed: {error}"),
    };

    let unclipped = DrawText {
        font_id,
        text: "MMMMMMMM".to_owned(),
        size_pixels: 32.0,
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
        origin: [8.0, 8.0],
        space: QuadSpace::Screen,
        clip: None,
    };
    harness
        .execute(vec![
            ("prime", HarnessOp::send_and_settle::<DrawText>("aether.text", &unclipped)),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("prime draw");

    let outside_region = (58, 18, 18, 18);
    let baseline = harness
        .execute(vec![("baseline", HarnessOp::capture_with_mails(vec![envelope("aether.text", &unclipped)], vec![]))])
        .expect("capture unclipped text");
    let baseline_img =
        decode_png(baseline.captured("baseline").expect("baseline ran")).expect("decode unclipped text png");
    let baseline_bg = background_top_left(&baseline_img);
    let tolerance = 5;
    let baseline_outside = lit_fraction_in_rect(
        &baseline_img,
        outside_region.0,
        outside_region.1,
        outside_region.2,
        outside_region.3,
        baseline_bg,
        tolerance,
    );
    assert!(
        baseline_outside > 0.05,
        "unclipped text should light the sampled outside region; coverage={baseline_outside}",
    );

    let clipped = DrawText { clip: Some(ClipRect { x: 18.0, y: 12.0, width: 22.0, height: 24.0 }), ..unclipped };
    let captured = harness
        .execute(vec![("clipped", HarnessOp::capture_with_mails(vec![envelope("aether.text", &clipped)], vec![]))])
        .expect("capture clipped text");
    let img = decode_png(captured.captured("clipped").expect("clipped ran")).expect("decode text png");
    let bg = background_top_left(&img);
    let inside = lit_fraction_in_rect(&img, 20, 18, 14, 14, bg, tolerance);
    let outside = lit_fraction_in_rect(
        &img,
        outside_region.0,
        outside_region.1,
        outside_region.2,
        outside_region.3,
        bg,
        tolerance,
    );
    assert!(inside > 0.05, "clipped text should still light pixels inside the clip; coverage={inside}");
    assert_eq!(outside, 0.0, "glyph pixels outside the text clip should remain background");
}

/// ADR-0105 font-metrics grab end to end (issue 1854): grab a real
/// font's size-independent metric table over the mail path — by path, so
/// the cap loads it on the miss — cache it guest-side, and assert the
/// local measurement of a run reproduces the cap's draw-path advance sum
/// bit-for-bit. That equality is the synchronous-local-layout invariant:
/// a consumer measures text without a per-measurement mail round trip and
/// still matches what the cap would draw.
///
/// CPU-only (no capture), but the harness still boots a render-composed
/// chassis, so it skips on driverless runners like the other scenarios.
#[test]
fn font_metrics_grab_measures_like_the_draw_path() {
    if !require_wgpu_only() {
        return;
    }

    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_actor::<TextCapability>(())
        .size(64, 32)
        .namespace_roots(font_namespace_roots())
        .build()
        .expect("boot");

    // Grab by path with no prior load — exercises load-on-miss.
    let grabbed = harness
        .execute(vec![(
            "grab",
            HarnessOp::send_and_await_reply(
                "aether.text",
                &FontMetricsRequest {
                    font: FontRef::Path { namespace: "assets".to_owned(), path: FONT_PATH.to_owned() },
                },
            ),
        )])
        .expect("font_metrics grab sequence");
    let metrics = match grabbed.reply::<FontMetricsResult>("grab").expect("decode FontMetricsResult") {
        FontMetricsResult::Ok { metrics } => metrics,
        FontMetricsResult::Err { error } => panic!("font_metrics failed: {error}"),
    };

    // Cache the table guest-side and measure a run locally.
    let cache = CachedFontMetrics::new(&metrics);
    let text = "Hello aether";
    let size = 29.0;
    let local = cache.measure(text, size);

    // Ground truth: fontdue's draw-path pen walk over the same string,
    // parsed from the same in-repo TTF the cap loaded.
    let ttf = fs::read(font_assets_root().join(FONT_PATH)).expect("read vendored Roboto Mono");
    let font = fontdue::Font::from_bytes(ttf, fontdue::FontSettings::default()).expect("vendored Roboto Mono parses");
    let mut draw_pen = 0.0f32;
    for ch in text.chars() {
        draw_pen += font.metrics(ch, size).advance_width;
    }

    assert!(local > 0.0, "a non-empty run has positive extent");
    assert_eq!(local, draw_pen, "local measure must equal the draw-path advance sum exactly");
}

/// ADR-0105 screen-space text origin (issue 1773): drawing `Screen` text
/// at a non-zero `origin` shifts the lit centroid by the offset, so the
/// string no longer sits at the window top-left.
///
/// Two captures back-to-back — one at `origin = [0, 0]` and one at
/// `origin = [ox, oy]` — are taken in the same harness session (font and
/// atlas are already live by the time the second capture fires). The
/// centroid of the offset capture must sit further right and further down
/// than the zero-origin centroid by at least half the applied offset,
/// ruling out a no-op implementation.
///
/// Skipped on driverless runners.
#[test]
#[allow(clippy::cast_precision_loss)]
fn text_screen_origin_shifts_centroid() {
    if !require_wgpu_only() {
        return;
    }

    let (frame_width, frame_height) = (256u32, 128u32);
    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_actor::<TextCapability>(())
        .size(frame_width, frame_height)
        .namespace_roots(font_namespace_roots())
        .build()
        .expect("boot");

    // Load the font.
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: FONT_PATH.to_owned() },
            ),
        )])
        .expect("load_font sequence");
    let font_id = match loaded.reply::<LoadFontResult>("load").expect("decode LoadFontResult") {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load_font failed: {error}"),
    };

    let draw_zero = DrawText {
        font_id,
        text: "Hi".to_owned(),
        size_pixels: 24.0,
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
        origin: [0.0, 0.0],
        space: QuadSpace::Screen,
        clip: None,
    };

    // Prime pass: lazily creates the atlas texture; nothing draws yet.
    harness
        .execute(vec![
            ("prime", HarnessOp::send_and_settle::<DrawText>("aether.text", &draw_zero)),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("prime draw");

    // Capture at origin [0, 0].
    let pre_zero = vec![envelope("aether.text", &draw_zero)];
    let snap_zero =
        harness.execute(vec![("snap0", HarnessOp::capture_with_mails(pre_zero, vec![]))]).expect("capture zero-origin");
    let img_zero = decode_png(snap_zero.captured("snap0").expect("snap0 ran")).expect("decode zero-origin png");
    let bg = background_top_left(&img_zero);
    let tolerance = 5;
    let base_center = centroid(&img_zero, bg, tolerance).expect("zero-origin frame has lit pixels");

    // Capture at a shifted origin — well inside the frame so glyphs render.
    let ox = (frame_width / 2) as f32;
    let oy = (frame_height / 2) as f32;
    let draw_offset = DrawText { origin: [ox, oy], ..draw_zero };
    let pre_offset = vec![envelope("aether.text", &draw_offset)];
    let snap_offset = harness
        .execute(vec![("snap1", HarnessOp::capture_with_mails(pre_offset, vec![]))])
        .expect("capture offset-origin");
    let img_offset = decode_png(snap_offset.captured("snap1").expect("snap1 ran")).expect("decode offset-origin png");
    let shifted_center = centroid(&img_offset, bg, tolerance).expect("offset-origin frame has lit pixels");

    // The shifted centroid must sit at least half the applied offset further
    // right and down — a strict half-delta guard that would catch a no-op.
    assert!(
        shifted_center.0 > base_center.0 + ox / 2.0,
        "offset centroid x={} should be right of zero centroid x={} \
         by at least {} (applied offset {ox})",
        shifted_center.0,
        base_center.0,
        ox / 2.0,
    );
    assert!(
        shifted_center.1 > base_center.1 + oy / 2.0,
        "offset centroid y={} should be below zero centroid y={} \
         by at least {} (applied offset {oy})",
        shifted_center.1,
        base_center.1,
        oy / 2.0,
    );
}

/// ADR-0105 World-space text (issue 1699): draws `World { anchor,
/// scale }` text under a perspective camera and asserts:
///
/// 1. `Distance { reference_distance: 10 }` labels shrink proportionally
///    as the camera dollies from d=10 to d=20 — bbox width ratio ≈ 0.5.
/// 2. `Pixels` labels hold their screen size across the same dolly —
///    bbox width ratio ≈ 1.0.
/// 3. The Pixels label stays axis-aligned at a 45-degree orbit angle
///    (bbox width within ±30% of the front-facing width), confirming the
///    clip-space approach never skews the label with the camera.
///
/// Skipped when no wgpu adapter is available (driverless CI runner).
#[test]
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn text_draws_world_space_label() {
    use std::f32::consts::PI;

    if !require_wgpu_only() {
        return;
    }

    let (frame_width, frame_height) = (128u32, 96u32);
    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_actor::<TextCapability>(())
        .size(frame_width, frame_height)
        .namespace_roots(font_namespace_roots())
        .build()
        .expect("boot");

    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: FONT_PATH.to_owned() },
            ),
        )])
        .expect("load_font sequence");
    let font_id = match loaded.reply::<LoadFontResult>("load").expect("decode LoadFontResult") {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load_font failed: {error}"),
    };

    // Build view-projection matrices for three camera positions.
    let fov_y = PI / 3.0;
    let aspect = frame_width as f32 / frame_height as f32;
    let proj = Mat4::perspective_rh(fov_y, aspect, 0.1, 100.0);
    let up = Vec3::new(0.0, 1.0, 0.0);

    let view_near = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 10.0), Vec3::ZERO, up);
    let view_far = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 20.0), Vec3::ZERO, up);
    let orbit_x = 10.0_f32 * (PI / 4.0).sin();
    let orbit_z = 10.0_f32 * (PI / 4.0).cos();
    let view_orbit = Mat4::look_at_rh(Vec3::new(orbit_x, 0.0, orbit_z), Vec3::ZERO, up);

    let vp_near = (proj * view_near).to_cols_array();
    let vp_far = (proj * view_far).to_cols_array();
    let vp_orbit = (proj * view_orbit).to_cols_array();

    let anchor = [0.0_f32, 0.0, 0.0];
    let draw_dist = DrawText {
        font_id,
        text: "Hy".to_owned(),
        size_pixels: 24.0,
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
        origin: [0.0, 0.0],
        space: QuadSpace::World { anchor, scale: QuadScale::Distance { reference_distance: 10.0 } },
        clip: None,
    };
    let draw_px = DrawText {
        font_id,
        text: "Hy".to_owned(),
        size_pixels: 24.0,
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
        origin: [0.0, 0.0],
        space: QuadSpace::World { anchor, scale: QuadScale::Pixels },
        clip: None,
    };

    // Prime: the first draw lazily creates the atlas texture and draws
    // nothing until the create_texture reply lands. Advance twice to
    // settle it so subsequent captures can render immediately.
    harness
        .execute(vec![
            (
                "cam",
                HarnessOp::send_and_settle::<ViewProjection>("aether.render", &ViewProjection { view_proj: vp_near }),
            ),
            ("prime", HarnessOp::send_and_settle::<DrawText>("aether.text", &draw_dist)),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("prime draw");

    let tol = 5u8;

    // Capture Distance label at near (d=10) and far (d=20).
    let snap_near = harness
        .execute(vec![(
            "s",
            HarnessOp::capture_with_mails(
                vec![
                    envelope("aether.render", &ViewProjection { view_proj: vp_near }),
                    envelope("aether.text", &draw_dist),
                ],
                vec![],
            ),
        )])
        .expect("near capture");
    let img_near = decode_png(snap_near.captured("s").expect("s ran")).expect("decode near");
    let bb_near = bounding_box(&img_near, background_top_left(&img_near), tol).expect("near frame has content");

    let snap_far = harness
        .execute(vec![(
            "s",
            HarnessOp::capture_with_mails(
                vec![
                    envelope("aether.render", &ViewProjection { view_proj: vp_far }),
                    envelope("aether.text", &draw_dist),
                ],
                vec![],
            ),
        )])
        .expect("far capture");
    let img_far = decode_png(snap_far.captured("s").expect("s ran")).expect("decode far");
    let bb_far = bounding_box(&img_far, background_top_left(&img_far), tol).expect("far frame has content");

    // Distance label at d=20 should be ~0.5x the width at d=10 because
    // k/clip.w = reference_distance/depth shrinks by half. Allow ±25%
    // slop for pixel-grid rounding.
    let near_w = (bb_near.max_x - bb_near.min_x + 1) as f32;
    let far_w = (bb_far.max_x - bb_far.min_x + 1) as f32;
    let dist_ratio = far_w / near_w;
    assert!(
        (0.25..0.75).contains(&dist_ratio),
        "Distance label width at d=20 / d=10 = {dist_ratio:.3} should be near 0.5 \
         (near={near_w}px, far={far_w}px); Distance scaling is broken",
    );

    // Capture Pixels label at near and far: width should hold constant.
    let snap_px_near = harness
        .execute(vec![(
            "s",
            HarnessOp::capture_with_mails(
                vec![
                    envelope("aether.render", &ViewProjection { view_proj: vp_near }),
                    envelope("aether.text", &draw_px),
                ],
                vec![],
            ),
        )])
        .expect("pixels-near capture");
    let img_px_near = decode_png(snap_px_near.captured("s").expect("s ran")).expect("decode px-near");
    let bb_px_near =
        bounding_box(&img_px_near, background_top_left(&img_px_near), tol).expect("px-near frame has content");

    let snap_px_far = harness
        .execute(vec![(
            "s",
            HarnessOp::capture_with_mails(
                vec![
                    envelope("aether.render", &ViewProjection { view_proj: vp_far }),
                    envelope("aether.text", &draw_px),
                ],
                vec![],
            ),
        )])
        .expect("pixels-far capture");
    let img_px_far = decode_png(snap_px_far.captured("s").expect("s ran")).expect("decode px-far");
    let bb_px_far = bounding_box(&img_px_far, background_top_left(&img_px_far), tol).expect("px-far frame has content");

    let px_near_w = (bb_px_near.max_x - bb_px_near.min_x + 1) as f32;
    let px_far_w = (bb_px_far.max_x - bb_px_far.min_x + 1) as f32;
    let px_ratio = px_far_w / px_near_w;
    assert!(
        (0.80..1.25).contains(&px_ratio),
        "Pixels label width at d=20 / d=10 = {px_ratio:.3} should be near 1.0 \
         (near={px_near_w}px, far={px_far_w}px); Pixels constant-size is broken",
    );

    // Orbit: a 45-degree horizontal orbit should not skew the label.
    // The Pixels-mode width at the orbit angle should be within ±30% of
    // the front-facing width — a true in-world quad would skew and widen
    // significantly.
    let snap_orbit = harness
        .execute(vec![(
            "s",
            HarnessOp::capture_with_mails(
                vec![
                    envelope("aether.render", &ViewProjection { view_proj: vp_orbit }),
                    envelope("aether.text", &draw_px),
                ],
                vec![],
            ),
        )])
        .expect("orbit capture");
    let img_orbit = decode_png(snap_orbit.captured("s").expect("s ran")).expect("decode orbit");
    let bb_orbit = bounding_box(&img_orbit, background_top_left(&img_orbit), tol).expect("orbit frame has content");

    let orbit_w = (bb_orbit.max_x - bb_orbit.min_x + 1) as f32;
    let orbit_ratio = orbit_w / px_near_w;
    assert!(
        (0.70..1.43).contains(&orbit_ratio),
        "Pixels label width at 45-degree orbit / front-facing = {orbit_ratio:.3} should be \
         near 1.0 (orbit={orbit_w}px, front={px_near_w}px); label may be skewing with camera",
    );
}
