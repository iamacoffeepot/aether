use super::*;

/// ADR-0105 text surface end to end: load a real OFL TTF through the
/// `assets` namespace, draw a `Screen`-space string, and assert the
/// captured frame lights a region in the upper-left where top-left-
/// anchored text lands. No component is loaded — the text is the only
/// thing that can light a pixel.
///
/// The first `draw` lazily creates the atlas texture (and draws nothing
/// that turn); `send_and_await` settles that `create_texture` round trip,
/// so the texture id is live before the capture's pre-mail `draw`
/// rasterizes glyphs and emits the quad batch.
#[test]
#[allow(clippy::cast_precision_loss)]
fn text_draws_a_screen_space_string() {
    // The crate's vendored Roboto Mono (SIL OFL 1.1) — copied into the
    // sandbox so the `assets` namespace can read it.
    const TTF: &[u8] = include_bytes!("../../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-text");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let (frame_width, frame_height) = (128u32, 64u32);
    let mut bench = SubstrateBench::builder()
        .size(frame_width, frame_height)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    // Load the font; the reply carries the session-scoped font_id.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: "font.ttf".to_owned() },
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
    bench
        .execute(vec![("prime", BenchOp::send_mail::<DrawText>("aether.text", &draw)), ("settle", BenchOp::advance(2))])
        .expect("prime draw");

    // Now the glyphs rasterize and the quad batch reaches the renderer the
    // same tick the capture records.
    let pre = vec![envelope("aether.text", &draw)];
    let captured = bench.execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))]).expect("capture-with-mails");
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

/// Issue #2855: `aether.text.draw` forwards its framebuffer clip to the
/// textured glyph quad batch it emits, so glyph pixels outside the clip
/// are discarded by the same overlay-pass scissor as primitive quads.
#[test]
#[allow(clippy::too_many_lines)]
fn text_draw_clip_bounds_glyph_pixels() {
    const TTF: &[u8] = include_bytes!("../../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-text-clip");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let mut bench =
        SubstrateBench::builder().size(128, 64).namespace_roots(test_namespace_roots(sandbox)).build().expect("boot");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: "font.ttf".to_owned() },
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
    bench
        .execute(vec![
            ("prime", BenchOp::send_mail::<DrawText>("aether.text", &unclipped)),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("prime draw");

    let outside_region = (58, 18, 18, 18);
    let baseline = bench
        .execute(vec![("baseline", BenchOp::capture_with_mails(vec![envelope("aether.text", &unclipped)], vec![]))])
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
    let captured = bench
        .execute(vec![("clipped", BenchOp::capture_with_mails(vec![envelope("aether.text", &clipped)], vec![]))])
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
/// CPU-only (no capture), but the bench still boots a full chassis, so it
/// skips on driverless runners like the other scenarios.
#[test]
fn font_metrics_grab_measures_like_the_draw_path() {
    const TTF: &[u8] = include_bytes!("../../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-font-metrics");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let mut bench =
        SubstrateBench::builder().size(64, 32).namespace_roots(test_namespace_roots(sandbox)).build().expect("boot");

    // Grab by path with no prior load — exercises load-on-miss.
    let grabbed = bench
        .execute(vec![(
            "grab",
            BenchOp::send_and_await(
                "aether.text",
                &FontMetricsRequest {
                    font: FontRef::Path { namespace: "assets".to_owned(), path: "font.ttf".to_owned() },
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

    // Ground truth: fontdue's draw-path pen walk over the same string.
    let font = fontdue::Font::from_bytes(TTF, fontdue::FontSettings::default()).expect("vendored Roboto Mono parses");
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
/// `origin = [ox, oy]` — are taken in the same bench session (font and
/// atlas are already live by the time the second capture fires). The
/// centroid of the offset capture must sit further right and further down
/// than the zero-origin centroid by at least half the applied offset,
/// ruling out a no-op implementation.
///
/// Skipped on driverless runners.
#[test]
#[allow(clippy::cast_precision_loss)]
fn text_screen_origin_shifts_centroid() {
    const TTF: &[u8] = include_bytes!("../../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-text-origin");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let (frame_width, frame_height) = (256u32, 128u32);
    let mut bench = SubstrateBench::builder()
        .size(frame_width, frame_height)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    // Load the font.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: "font.ttf".to_owned() },
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
    bench
        .execute(vec![
            ("prime", BenchOp::send_mail::<DrawText>("aether.text", &draw_zero)),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("prime draw");

    // Capture at origin [0, 0].
    let pre_zero = vec![envelope("aether.text", &draw_zero)];
    let snap_zero =
        bench.execute(vec![("snap0", BenchOp::capture_with_mails(pre_zero, vec![]))]).expect("capture zero-origin");
    let img_zero = decode_png(snap_zero.captured("snap0").expect("snap0 ran")).expect("decode zero-origin png");
    let bg = background_top_left(&img_zero);
    let tolerance = 5;
    let base_center = centroid(&img_zero, bg, tolerance).expect("zero-origin frame has lit pixels");

    // Capture at a shifted origin — well inside the frame so glyphs render.
    let ox = (frame_width / 2) as f32;
    let oy = (frame_height / 2) as f32;
    let draw_offset = DrawText { origin: [ox, oy], ..draw_zero };
    let pre_offset = vec![envelope("aether.text", &draw_offset)];
    let snap_offset =
        bench.execute(vec![("snap1", BenchOp::capture_with_mails(pre_offset, vec![]))]).expect("capture offset-origin");
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
/// Skipped when no wgpu adapter is available (driverless CI runner) or
/// the font asset hasn't been staged.
#[test]
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn text_draws_world_space_label() {
    use std::f32::consts::PI;

    const TTF: &[u8] = include_bytes!("../../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-world-text");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let (frame_width, frame_height) = (128u32, 96u32);
    let mut bench = SubstrateBench::builder()
        .size(frame_width, frame_height)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: "font.ttf".to_owned() },
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
    bench
        .execute(vec![
            ("cam", BenchOp::send_mail::<ViewProjection>("aether.render", &ViewProjection { view_proj: vp_near })),
            ("prime", BenchOp::send_mail::<DrawText>("aether.text", &draw_dist)),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("prime draw");

    let tol = 5u8;

    // Capture Distance label at near (d=10) and far (d=20).
    let snap_near = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
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

    let snap_far = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
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
    let snap_px_near = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
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

    let snap_px_far = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
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
    let snap_orbit = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
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
