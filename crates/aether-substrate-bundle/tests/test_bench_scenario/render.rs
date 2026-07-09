use super::*;

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
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    // Capture's frame runs without a dispatched tick, so the probe
    // won't auto-tick during the captured frame. The pre-mail bundle
    // wires it up: `set_render` flips state to "visible red", and a
    // synthesised `aether.lifecycle.tick` drives the probe's on_tick
    // to emit a `DrawTriangle` into the frame buffer right before the
    // GPU readback. The after-mail bundle flips render back to
    // invisible after the readback.
    let pre = vec![
        envelope(
            &probe_address(),
            &SetRender {
                r: 200,
                g: 32,
                b: 32,
                visible: 1,
            },
        ),
        NamedMail {
            recipient_name: probe_address(),
            kind_name: "aether.lifecycle.tick".to_owned(),
            payload: Vec::new(),
            count: 1,
        },
    ];
    let after = vec![envelope(
        &probe_address(),
        &SetRender {
            r: 0,
            g: 0,
            b: 0,
            visible: 0,
        },
    )];

    // Priming advance subscribes the probe to ticks; the
    // capture-with-mails op then dispatches the pre bundle, reads
    // back, and dispatches the after bundle — all in one frame.
    let captured = bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            ("snap", BenchOp::capture_with_mails(pre, after)),
        ])
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
        center_x > 0.1 * width
            && center_x < 0.9 * width
            && center_y > 0.1 * height
            && center_y < 0.9 * height,
        "triangle centroid ({center_x}, {center_y}) should sit in the frame interior \
         of the {}x{} capture",
        img.width,
        img.height,
    );

    // Cleanup ran: probe.render is now { visible: 0 }. Advance once
    // and capture again — the next tick won't emit DrawTriangle, so
    // the frame stays at clear color.
    let cleaned = bench
        .execute(vec![
            ("cleanup_advance", BenchOp::advance(1)),
            ("snap2", BenchOp::capture()),
        ])
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
    let mut bench = TestBench::start_with_size(width, height).expect("boot");
    load_cube(&mut bench, &wasm_path);

    // Priming advance subscribes the cube to ticks; the next tick (run
    // inside `capture`) drives the cube's camera + geometry emission so
    // the readback sees a fully-formed frame.
    let captured = bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            ("snap", BenchOp::capture()),
        ])
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
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

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

    let created = bench
        .execute(vec![(
            "create",
            BenchOp::send_and_await(
                "aether.render",
                &CreateTexture {
                    width: texture_width,
                    height: texture_height,
                    format: TextureFormat::Rgba8,
                    pixels,
                },
            ),
        )])
        .expect("create_texture sequence");
    let texture_id = match created
        .reply::<CreateTextureResult>("create")
        .expect("decode CreateTextureResult")
    {
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

    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture-with-mails");
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
    let cleared = bench
        .execute(vec![
            ("clear_advance", BenchOp::advance(1)),
            ("snap2", BenchOp::capture()),
        ])
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
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

    let texture_width = 8u32;
    let texture_height = 8u32;
    let pixels = vec![255u8; (texture_width * texture_height * 4) as usize];
    let created = bench
        .execute(vec![(
            "create",
            BenchOp::send_and_await(
                "aether.render",
                &CreateTexture {
                    width: texture_width,
                    height: texture_height,
                    format: TextureFormat::Rgba8,
                    pixels,
                },
            ),
        )])
        .expect("create_texture sequence");
    let texture_id = match created
        .reply::<CreateTextureResult>("create")
        .expect("decode CreateTextureResult")
    {
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

    let captured = bench
        .execute(vec![(
            "snap",
            BenchOp::capture_with_mails(vec![draw()], vec![]),
        )])
        .expect("capture with live texture");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let drawn = coverage(&img, bg, 5);
    assert!(
        (0.08..0.22).contains(&drawn),
        "live texture quad coverage {drawn} fell outside the expected band",
    );

    let destroyed = bench
        .execute(vec![
            (
                "destroy",
                BenchOp::send_mail("aether.render", &DestroyTexture { texture_id }),
            ),
            ("advance", BenchOp::advance(1)),
            ("snap2", BenchOp::capture_with_mails(vec![draw()], vec![])),
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
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

    let texture_width = 8u32;
    let texture_height = 4u32;
    let mut pixels = vec![32u8; (texture_width * texture_height) as usize];
    let created = bench
        .execute(vec![(
            "create",
            BenchOp::send_and_await(
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
    let texture_id = match created
        .reply::<CreateTextureResult>("create")
        .expect("decode CreateTextureResult")
    {
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
            &UpdateTexture {
                texture_id,
                x: update_width,
                y: 0,
                width: update_width,
                height: update_height,
                pixels,
            },
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

    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture r8 texture");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    assert_eq!((img.width, img.height), (frame_width, frame_height));

    let sample = |x: u32, y: u32| -> [u8; 4] {
        let start = ((y * img.width + x) * 4) as usize;
        [
            img.rgba[start],
            img.rgba[start + 1],
            img.rgba[start + 2],
            img.rgba[start + 3],
        ]
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
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let pixels = vec![
        0, 0, 0, 0, 128, 128, 255, 255, //
        0, 0, 0, 0, 128, 128, 255, 255, //
        0, 0, 0, 0, 128, 128, 255, 255, //
        0, 0, 0, 0, 128, 128, 255, 255,
    ];
    let created = bench
        .execute(vec![(
            "create",
            BenchOp::send_and_await(
                "aether.render",
                &CreateTexture {
                    width: 8,
                    height: 4,
                    format: TextureFormat::R8,
                    pixels,
                },
            ),
        )])
        .expect("create coverage texture");
    let texture_id = match created
        .reply::<CreateTextureResult>("create")
        .expect("decode CreateTextureResult")
    {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    let pre = vec![envelope(
        "aether.render",
        &DrawMaterialCoverage {
            texture_id,
            rects: vec![MaterialCoverageRect {
                rect: MaterialRect {
                    x: -0.8,
                    y: -0.6,
                    width: 1.6,
                    height: 1.2,
                    z: 0.5,
                },
                body_color: Rgba::new(0.0, 0.9, 0.1, 1.0),
                rim_color: Rgba::new(1.0, 0.9, 0.0, 1.0),
                rim_width: 0.25,
            }],
        },
    )];
    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture coverage material");
    let img = decode_png(captured.captured("snap").expect("snap step ran"))
        .expect("decode coverage material png");
    let bg = background_top_left(&img);
    let outside = rgba_at(&img, 12, 24);
    let rim = rgba_at(&img, 38, 24);
    let body = rgba_at(&img, 48, 24);

    assert!(
        outside[0].abs_diff(bg[0]) <= 8
            && outside[1].abs_diff(bg[1]) <= 8
            && outside[2].abs_diff(bg[2]) <= 8,
        "outside coverage sample should stay background; bg={bg:?} outside={outside:?}",
    );
    assert!(
        rim[0] > 150 && rim[1] > 120 && rim[2] < 80,
        "coverage rim sample should be yellow; got {rim:?}",
    );
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
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let pixels = vec![255u8, 255, 255, 255];
    let created = bench
        .execute(vec![(
            "create",
            BenchOp::send_and_await(
                "aether.render",
                &CreateTexture {
                    width: 1,
                    height: 1,
                    format: TextureFormat::Rgba8,
                    pixels,
                },
            ),
        )])
        .expect("create textured material texture");
    let texture_id = match created
        .reply::<CreateTextureResult>("create")
        .expect("decode CreateTextureResult")
    {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    let occluder = DrawTriangle {
        verts: [
            Vertex {
                x: -0.9,
                y: -0.8,
                z: 0.0,
                color: Rgb::new(0.9, 0.0, 0.0),
            },
            Vertex {
                x: -0.9,
                y: 0.8,
                z: 0.0,
                color: Rgb::new(0.9, 0.0, 0.0),
            },
            Vertex {
                x: 0.0,
                y: 0.8,
                z: 0.0,
                color: Rgb::new(0.9, 0.0, 0.0),
            },
        ],
    };
    let pre = vec![
        envelope("aether.render", &occluder),
        envelope(
            "aether.render",
            &DrawMaterialTextured {
                texture_id,
                rects: vec![MaterialTexturedRect {
                    rect: MaterialRect {
                        x: -0.8,
                        y: -0.6,
                        width: 1.6,
                        height: 1.2,
                        z: 0.5,
                    },
                    u0: 0.0,
                    v0: 0.0,
                    u1: 1.0,
                    v1: 1.0,
                    tint: Rgba::new(0.0, 0.1, 1.0, 1.0),
                }],
            },
        ),
    ];
    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture textured material");
    let img = decode_png(captured.captured("snap").expect("snap step ran"))
        .expect("decode textured material png");
    let left = rgba_at(&img, 12, 20);
    let right = rgba_at(&img, 48, 24);
    assert!(
        left[0] > left[2].saturating_add(80),
        "left sample should show red main-pass occluder, not blue material; got {left:?}",
    );
    assert!(
        right[2] > right[0].saturating_add(100),
        "right sample should show blue textured material; got {right:?}",
    );
}

/// ADR-0140 coverage material rejects non-R8 textures at encode time:
/// the batch warn-drops, the frame still renders, and no material pixels
/// appear.
#[test]
fn coverage_material_warn_drops_non_r8_texture() {
    if !require_wgpu_only() {
        return;
    }
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let created = bench
        .execute(vec![(
            "create",
            BenchOp::send_and_await(
                "aether.render",
                &CreateTexture {
                    width: 2,
                    height: 2,
                    format: TextureFormat::Rgba8,
                    pixels: vec![255u8; 16],
                },
            ),
        )])
        .expect("create rgba texture");
    let texture_id = match created
        .reply::<CreateTextureResult>("create")
        .expect("decode CreateTextureResult")
    {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };
    let pre = vec![envelope(
        "aether.render",
        &DrawMaterialCoverage {
            texture_id,
            rects: vec![MaterialCoverageRect {
                rect: MaterialRect {
                    x: -0.8,
                    y: -0.6,
                    width: 1.6,
                    height: 1.2,
                    z: 0.5,
                },
                body_color: Rgba::new(0.0, 1.0, 0.0, 1.0),
                rim_color: Rgba::new(1.0, 1.0, 0.0, 1.0),
                rim_width: 0.25,
            }],
        },
    )];
    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture non-r8 coverage");
    let img = decode_png(captured.captured("snap").expect("snap step ran"))
        .expect("decode non-r8 coverage png");
    let drawn = coverage(&img, background_top_left(&img), 5);
    assert!(
        drawn < 0.01,
        "coverage draw against RGBA8 should be warn-dropped, but lit coverage was {drawn}",
    );
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
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

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

    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture-with-mails");
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
        cx >= quad_x - pad
            && cx <= quad_x + quad_w + pad
            && cy >= quad_y - pad
            && cy <= quad_y + quad_h + pad,
        "solid quad centroid ({cx}, {cy}) should sit inside the screen rect \
         ({quad_x},{quad_y})+({quad_w}x{quad_h}) of the {frame_width}x{frame_height} frame",
    );

    // Immediate-mode clear: advance with no quad resent, next capture returns to clear color.
    let cleared = bench
        .execute(vec![
            ("clear_advance", BenchOp::advance(1)),
            ("snap2", BenchOp::capture()),
        ])
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
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let clipped = envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: Some(ClipRect {
                x: 20.0,
                y: 12.0,
                width: 12.0,
                height: 10.0,
            }),
            quads: vec![SolidQuad {
                x: 10.0,
                y: 8.0,
                width: 44.0,
                height: 30.0,
                color: Rgba::new(1.0, 0.0, 0.0, 1.0),
            }],
        },
    );
    let unclipped = envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![SolidQuad {
                x: 44.0,
                y: 30.0,
                width: 8.0,
                height: 8.0,
                color: Rgba::new(0.0, 1.0, 0.0, 1.0),
            }],
        },
    );

    let captured = bench
        .execute(vec![(
            "snap",
            BenchOp::capture_with_mails(vec![clipped, unclipped], vec![]),
        )])
        .expect("capture clipped solid quads");
    let img = decode_png(captured.captured("snap").expect("snap step ran"))
        .expect("decode clipped solid png");
    let bg = background_top_left(&img);
    let tolerance = 5;
    assert!(
        pixel_is_lit(&img, 24, 16, bg, tolerance),
        "pixel inside the solid clip rect should be painted",
    );
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
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let created = bench
        .execute(vec![(
            "create",
            BenchOp::send_and_await(
                "aether.render",
                &CreateTexture {
                    width: 1,
                    height: 1,
                    format: TextureFormat::Rgba8,
                    pixels: vec![255, 255, 255, 255],
                },
            ),
        )])
        .expect("create white texture");
    let texture_id = match created
        .reply::<CreateTextureResult>("create")
        .expect("decode CreateTextureResult")
    {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };
    let draw = envelope(
        "aether.render",
        &DrawTexturedQuads {
            texture_id,
            space: QuadSpace::Screen,
            clip: Some(ClipRect {
                x: 18.0,
                y: 14.0,
                width: 14.0,
                height: 12.0,
            }),
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

    let captured = bench
        .execute(vec![(
            "snap",
            BenchOp::capture_with_mails(vec![draw], vec![]),
        )])
        .expect("capture clipped textured quad");
    let img = decode_png(captured.captured("snap").expect("snap step ran"))
        .expect("decode clipped textured png");
    let bg = background_top_left(&img);
    let tolerance = 5;
    assert!(
        pixel_is_lit(&img, 24, 20, bg, tolerance),
        "pixel inside the textured clip rect should be painted",
    );
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
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

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

    let result = bench
        .execute(vec![(
            "snap",
            BenchOp::send_and_await(
                "aether.render",
                &CaptureFrame {
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
        .expect("send_and_await(CaptureFrame) with checks");
    let reply: CaptureFrameResult = result.reply("snap").expect("decode CaptureFrameResult");
    let verdict = match reply {
        CaptureFrameResult::Ok { png, verdict, .. } => {
            assert!(
                png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
                "the PNG still rides back alongside the verdict",
            );
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
            assert!(
                (0.08..0.22).contains(fraction),
                "solid quad coverage {fraction} fell outside the expected band",
            );
        }
        other => panic!("expected Coverage result, got {other:?}"),
    }
    match &verdict.results[2] {
        FrameCheckResult::Centroid { centroid, .. } => {
            let [cx, cy] = centroid.expect("a lit frame has a centroid");
            let pad = 4.0f32;
            assert!(
                cx >= quad_x - pad
                    && cx <= quad_x + quad_w + pad
                    && cy >= quad_y - pad
                    && cy <= quad_y + quad_h + pad,
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
/// its reference image from the `TestBench`'s configured `assets`
/// namespace root, the same way the desktop chassis wires
/// `RenderConfig.assets_dir`. Captures a deterministic clear-color
/// frame, stores that exact PNG under the sandbox's assets root as the
/// reference, then requests a second capture with a `SimilarityCheck`
/// against it. Two captures of the same unchanged scene are pixel-
/// identical, so the score is `0.0` and the check passes — proving
/// `TestBenchChassis::build_passive` no longer leaves `assets_dir`
/// unconditionally `None` (the bug this issue fixes; on unfixed `main`
/// this fails at reference resolution with "no assets directory is
/// configured").
#[test]
fn capture_frame_similarity_resolves_reference_from_configured_assets_root() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-render-similarity");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let reference = bench
        .execute(vec![("reference", BenchOp::capture())])
        .expect("capture reference frame");
    let reference_png = reference
        .captured("reference")
        .expect("reference step ran")
        .to_vec();
    let reference_path = "similarity-reference.png";
    fs::write(sandbox.join(reference_path), &reference_png)
        .expect("write reference png under the sandbox assets root");

    let result = bench
        .execute(vec![(
            "snap",
            BenchOp::send_and_await(
                "aether.render",
                &CaptureFrame {
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
        .expect("send_and_await(CaptureFrame) with similarity");
    let reply: CaptureFrameResult = result.reply("snap").expect("decode CaptureFrameResult");
    match reply {
        CaptureFrameResult::Ok {
            png,
            verdict,
            similarity_score,
            similarity_pass,
        } => {
            assert!(
                png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
                "the PNG still rides back alongside the similarity score",
            );
            assert!(
                verdict.is_none(),
                "no checks were requested, so no intrinsic verdict should ride back",
            );
            assert_eq!(
                similarity_score,
                Some(0.0),
                "an unchanged scene captured twice should score a perfect match",
            );
            assert_eq!(
                similarity_pass,
                Some(true),
                "a 0.0 score against a 0.0 threshold must pass",
            );
        }
        CaptureFrameResult::Err { error } => panic!(
            "capture_frame similarity replied Err (assets root not wired into TestBench?): \
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
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

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
    let region = FrameRect {
        min_x: 4,
        min_y: 4,
        max_x: 15,
        max_y: 15,
    };
    let region_check = |reduction| FrameCheck {
        reduction,
        tolerance,
        background: None,
        region: Some(region),
    };

    let result = bench
        .execute(vec![(
            "snap",
            BenchOp::send_and_await(
                "aether.render",
                &CaptureFrame {
                    mails: vec![draw],
                    after_mails: vec![],
                    checks: vec![
                        region_check(FrameReduction::Coverage),
                        region_check(FrameReduction::Centroid),
                    ],
                    similarity: None,
                },
            ),
        )])
        .expect("send_and_await(CaptureFrame) with region-scoped checks");
    let reply: CaptureFrameResult = result.reply("snap").expect("decode CaptureFrameResult");
    let verdict = match reply {
        CaptureFrameResult::Ok { verdict, .. } => {
            verdict.expect("a checks request returns a verdict")
        }
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
                cx >= first_x
                    && cx <= first_x + first_w
                    && cy >= first_y
                    && cy <= first_y + first_h,
                "region-scoped centroid ({cx}, {cy}) should sit inside the first quad's rect, \
                 not blended toward the second quad the region excludes",
            );
        }
        other => panic!("expected Centroid result, got {other:?}"),
    }
}
