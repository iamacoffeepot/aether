//! Draw-pass harness scenarios (ADR-0171, issue #4385): the
//! `PassStage::Draw` arm driven end-to-end through an in-process
//! `SubstrateHarness` — a rasterized triangle observed in pixels, two
//! consecutive draw passes sharing a depth transient, the register-time
//! vertex/layout classes, and a dispatch naming a geometry id that does
//! not exist.
//!
//! Every pixel scenario observes the program's writable output texture
//! by drawing it through the overlay path in the same captured frame,
//! the readback route the fragment-pass scenarios already use.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, pixel_is_lit, rgba_at};
use aether_harness_substrate_capture::visual::{background_top_left, decode_png};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::QuadBlend;
use aether_render::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DrawPass, DrawSolidQuads,
    DrawTexturedQuads, GeometrySlotSpec, OutputSlot, PassLoad, PassStage, ProgramDispatch, ProgramPass,
    ProgramRegister, ProgramRegisterResult, SlotExtent, SlotSpec, SolidQuad, TextureFormat, TextureSampling,
    TextureUsage, TexturedQuad, VertexAttribute, VertexFormat,
};

/// Skip (or panic under `AETHER_REQUIRE_RUNTIME`) when no wgpu adapter
/// is available — every scenario here rasterizes for real.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// The shared draw module. `vs_flat` reads the position attribute for
/// x and y but takes clip depth from the uniform window, so the pass's
/// window reaching the *vertex* stage is what places the geometry in
/// depth; `fs_flat` paints the window's flat color.
const MODULE: &str = r"
struct DrawParams {
    color: vec4<f32>,
    depth: f32,
}
@group(0) @binding(0) var<uniform> draw_params: DrawParams;

@vertex
fn vs_flat(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position.xy, draw_params.depth, 1.0);
}

@vertex
fn vs_tinted(@location(0) position: vec3<f32>, @location(1) tint: vec4<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position.xy, tint.x, 1.0);
}

@fragment
fn fs_flat() -> @location(0) vec4<f32> {
    return draw_params.color;
}
";

/// Bytes of the module's `DrawParams` block: a `vec4<f32>` then an
/// `f32`, padded out to the struct's 16-byte alignment.
const DRAW_PARAMS_BYTES: usize = 32;

/// Side of every program output texture, and of the overlay quad that
/// reads it back at 2x.
const OUTPUT_SIDE: u32 = 16;

/// Where the readback quad sits in the 64x48 frame, and how big it is.
const QUAD_ORIGIN: (f32, f32) = (16.0, 8.0);
const QUAD_SIDE: f32 = 32.0;

fn position_slot() -> GeometrySlotSpec {
    GeometrySlotSpec { layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }] }
}

/// One pass's uniform window: a flat color and the clip depth the
/// vertex stage places every vertex at.
fn draw_params(color: [f32; 4], depth: f32) -> Vec<u8> {
    let mut window: Vec<u8> = color.iter().flat_map(|channel| channel.to_le_bytes()).collect();
    window.extend_from_slice(&depth.to_le_bytes());
    window.resize(DRAW_PARAMS_BYTES, 0);
    window
}

fn draw_pass(geometry: u32, depth: Option<u32>, load: PassLoad, uniform_offset: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Draw(DrawPass { vertex_entry_point: "vs_flat".to_owned(), geometry, depth, load }),
        entry_point: "fs_flat".to_owned(),
        inputs: Vec::new(),
        output: OutputSlot::Binding { index: 0 },
        uniform_offset,
        uniform_length: u32::try_from(DRAW_PARAMS_BYTES).expect("window length fits u32"),
        repeat: None,
    }
}

fn create_texture(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateTexture) -> u32 {
    let created = harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("create_texture sequence");
    match created.reply::<CreateTextureResult>(label).expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture ({label}) failed: {error}"),
    }
}

/// The writable `Rgba8` texture every program here draws into.
fn create_output(harness: &mut SubstrateHarness) -> u32 {
    create_texture(
        harness,
        "create_output",
        &CreateTexture {
            width: OUTPUT_SIDE,
            height: OUTPUT_SIDE,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        },
    )
}

/// Upload one position-only indexed triangle list. Positions are clip
/// space in x and y; the vertex stage replaces z with the pass's
/// uniform depth.
fn create_geometry(
    harness: &mut SubstrateHarness,
    label: &'static str,
    positions: &[[f32; 3]],
    indices: &[u32],
) -> u32 {
    let mail = CreateGeometry {
        layout: position_slot().layout,
        vertices: positions.iter().flatten().flat_map(|value| value.to_le_bytes()).collect(),
        indices: indices.iter().flat_map(|index| index.to_le_bytes()).collect(),
    };
    let created = harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", &mail))])
        .expect("create_geometry sequence");
    match created.reply::<CreateGeometryResult>(label).expect("decode CreateGeometryResult") {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { reason } => panic!("create_geometry ({label}) failed: {reason}"),
    }
}

/// A clip-space rectangle spanning `left..right` in x and the full
/// height, as four vertices and two triangles.
fn quad_geometry(left: f32, right: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
    (vec![[left, -1.0, 0.0], [right, -1.0, 0.0], [right, 1.0, 0.0], [left, 1.0, 0.0]], vec![0, 1, 2, 0, 2, 3])
}

fn register_reply(
    harness: &mut SubstrateHarness,
    label: &'static str,
    mail: &ProgramRegister,
) -> ProgramRegisterResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("register sequence")
        .reply::<ProgramRegisterResult>(label)
        .expect("decode ProgramRegisterResult")
}

fn registered_id(harness: &mut SubstrateHarness, label: &'static str, mail: &ProgramRegister) -> u32 {
    match register_reply(harness, label, mail) {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register ({label}) failed: {reason}"),
    }
}

fn register_err(harness: &mut SubstrateHarness, label: &'static str, mail: &ProgramRegister) -> String {
    match register_reply(harness, label, mail) {
        ProgramRegisterResult::Err { reason } => reason,
        ProgramRegisterResult::Ok { program_id } => panic!("register ({label}) must reject; got program {program_id}"),
    }
}

/// An overlay draw of the program's output as a `QUAD_SIDE` screen rect
/// at `QUAD_ORIGIN` — how the pixel scenarios observe it.
fn output_overlay(texture_id: u32) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        layer: 0,
        quads: vec![TexturedQuad {
            x: QUAD_ORIGIN.0,
            y: QUAD_ORIGIN.1,
            width: QUAD_SIDE,
            height: QUAD_SIDE,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

/// A small white quad in the frame's top-left corner: proof that the
/// frame's own passes ran when a scenario expects a dispatch to drop.
fn control_quad() -> DrawSolidQuads {
    DrawSolidQuads {
        space: QuadSpace::Screen,
        clip: None,
        layer: 0,
        quads: vec![SolidQuad { x: 2.0, y: 2.0, width: 5.0, height: 5.0, color: Rgba::new(1.0, 1.0, 1.0, 1.0) }],
    }
}

/// ADR-0171: one draw pass rasterizes a bound geometry into a writable
/// output — a centered clip-space triangle lands lit where the triangle
/// covers and leaves the rest of the output cleared, so the overlay
/// blends nothing there. The named bugs: the draw pass never recording
/// (a blank output, both probes at background); a vertex buffer layout
/// built with the wrong stride or attribute offset, which scatters the
/// triangle's corners and moves the covered region; the pass's clear
/// semantics inverted so the untouched corner carries color; and the
/// index buffer bound with the wrong format, which draws nothing or
/// draws garbage indices.
#[test]
fn draw_pass_rasterizes_a_triangle_into_its_output() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let geometry_id = create_geometry(
        &mut harness,
        "create_triangle",
        &[[-0.8, -0.8, 0.0], [0.8, -0.8, 0.0], [0.0, 0.8, 0.0]],
        &[0, 1, 2],
    );
    let output_id = create_output(&mut harness);
    let program_id = registered_id(
        &mut harness,
        "register",
        &ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
            transients: Vec::new(),
            geometries: vec![position_slot()],
            depth_transients: Vec::new(),
            passes: vec![draw_pass(0, None, PassLoad::Clear, 0)],
        },
    );

    let pre = vec![
        envelope(
            "aether.render",
            &ProgramDispatch {
                program_id,
                bindings: vec![output_id],
                geometries: vec![geometry_id],
                uniforms: draw_params([1.0, 1.0, 1.0, 1.0], 0.5),
            },
        ),
        envelope("aether.render", &output_overlay(output_id)),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture draw output");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode draw capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // The quad maps clip x/y linearly onto screen 16..48 / 8..40. The
    // inside probe sits at clip (0, -0.4), well within the triangle;
    // the outside probe at clip (-0.8, +0.8), where only the apex
    // reaches and the triangle does not cover.
    assert!(pixel_is_lit(&img, 32, 30, bg, tolerance), "the triangle's interior must be painted by the draw pass");
    let outside = rgba_at(&img, 19, 11);
    assert!(
        outside[0].abs_diff(bg[0]) <= tolerance
            && outside[1].abs_diff(bg[1]) <= tolerance
            && outside[2].abs_diff(bg[2]) <= tolerance,
        "outside the triangle the output stays cleared, so the frame's background shows through; \
         bg={bg:?} probe={outside:?}",
    );
}

/// ADR-0171 depth sharing: two consecutive draw passes naming one depth
/// transient agree on occlusion. The near quad is drawn *first* at clip
/// depth 0.2 and the far quad second at 0.8, so only a working depth
/// test can keep the near one on top where they overlap — without it
/// the later pass simply paints over. The named bugs: the depth
/// attachment omitted or the comparison inverted (the overlap turns the
/// far color); the second pass re-clearing the shared depth instead of
/// loading it (the overlap turns the far color again, which is why the
/// left-only probe is asserted too — it separates "depth ignored" from
/// "second pass never ran"); and the second pass clearing the color
/// output despite declaring `Load`, which would erase the near quad
/// everywhere.
#[test]
fn consecutive_draw_passes_share_depth_and_occlude() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let (near_positions, indices) = quad_geometry(-1.0, 0.2);
    let near_id = create_geometry(&mut harness, "create_near", &near_positions, &indices);
    let (far_positions, _) = quad_geometry(-0.2, 1.0);
    let far_id = create_geometry(&mut harness, "create_far", &far_positions, &indices);
    let output_id = create_output(&mut harness);
    let window_bytes = u32::try_from(DRAW_PARAMS_BYTES).expect("window length fits u32");
    let program_id = registered_id(
        &mut harness,
        "register",
        &ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
            transients: Vec::new(),
            geometries: vec![position_slot(), position_slot()],
            depth_transients: vec![SlotExtent::Full],
            passes: vec![
                draw_pass(0, Some(0), PassLoad::Clear, 0),
                draw_pass(1, Some(0), PassLoad::Load, window_bytes),
            ],
        },
    );

    let mut uniforms = draw_params([1.0, 0.0, 0.0, 1.0], 0.2);
    uniforms.extend(draw_params([0.0, 1.0, 0.0, 1.0], 0.8));
    let pre = vec![
        envelope(
            "aether.render",
            &ProgramDispatch { program_id, bindings: vec![output_id], geometries: vec![near_id, far_id], uniforms },
        ),
        envelope("aether.render", &output_overlay(output_id)),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture depth output");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode depth capture png");

    // Screen 22 is inside the near quad alone, 32 inside the overlap,
    // 42 inside the far quad alone; row 24 is the vertical middle.
    let near_only = rgba_at(&img, 22, 24);
    let overlap = rgba_at(&img, 32, 24);
    let far_only = rgba_at(&img, 42, 24);
    assert!(near_only[0] > near_only[1] + 60, "the near quad's own region must be red; got {near_only:?}");
    assert!(far_only[1] > far_only[0] + 60, "the far quad's own region must be green; got {far_only:?}");
    assert!(
        overlap[0] > overlap[1] + 60,
        "the near quad was drawn first at depth 0.2, so the shared depth test must keep it over the far quad at \
         0.8 in the overlap; got {overlap:?}",
    );
}

/// ADR-0171 register validation: the draw classes reply their own
/// distinguishable `Err` reasons over the mail path — a vertex stage
/// reading a location the bound layout does not declare, and one
/// reading a declared location as the wrong WGSL type. The named bugs:
/// either mismatch reaching wgpu as an opaque `pipeline creation
/// failed` (an author cannot tell which attribute is wrong), or worse
/// passing validation and feeding the vertex stage bytes that mean
/// something else.
#[test]
fn vertex_layout_mismatch_replies_a_distinguishable_error() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let base = || ProgramRegister {
        wgsl: MODULE.to_owned(),
        bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        transients: Vec::new(),
        geometries: vec![position_slot()],
        depth_transients: Vec::new(),
        passes: vec![draw_pass(0, None, PassLoad::Clear, 0)],
    };

    let undeclared_location = register_err(
        &mut harness,
        "undeclared_location",
        &ProgramRegister {
            passes: vec![ProgramPass {
                stage: PassStage::Draw(DrawPass {
                    vertex_entry_point: "vs_tinted".to_owned(),
                    geometry: 0,
                    depth: None,
                    load: PassLoad::Clear,
                }),
                ..draw_pass(0, None, PassLoad::Clear, 0)
            }],
            ..base()
        },
    );
    assert!(
        undeclared_location.contains("@location(1)") && undeclared_location.contains("does not declare"),
        "the unbound-location class must be named; got: {undeclared_location}",
    );

    let wrong_format = register_err(
        &mut harness,
        "wrong_format",
        &ProgramRegister {
            geometries: vec![GeometrySlotSpec {
                layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x2 }],
            }],
            ..base()
        },
    );
    assert!(
        wrong_format.contains("consumed as vec2<f32>"),
        "the format-mismatch class must name the type the layout implies; got: {wrong_format}",
    );

    match register_reply(&mut harness, "accepted", &base()) {
        ProgramRegisterResult::Ok { program_id } => {
            assert_eq!(program_id, 0, "rejected registers must not consume ids");
        }
        ProgramRegisterResult::Err { reason } => panic!("the matching draw program must register: {reason}"),
    }
}

/// ADR-0171 runtime mismatch: a dispatch naming a geometry id that was
/// never created warn-drops the whole dispatch and the frame survives —
/// the capture succeeds, the control quad draws, and the program's
/// output keeps its cleared content. The named bug: the unknown id
/// reaching the record path, where resolving it would panic the driver
/// thread and take the frame (no capture, no control quad) rather than
/// dropping one dispatch.
#[test]
fn unknown_geometry_id_drops_and_frame_survives() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let output_id = create_output(&mut harness);
    let program_id = registered_id(
        &mut harness,
        "register",
        &ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
            transients: Vec::new(),
            geometries: vec![position_slot()],
            depth_transients: Vec::new(),
            passes: vec![draw_pass(0, None, PassLoad::Clear, 0)],
        },
    );

    let pre = vec![
        envelope(
            "aether.render",
            &ProgramDispatch {
                program_id,
                bindings: vec![output_id],
                geometries: vec![4242],
                uniforms: draw_params([1.0, 1.0, 1.0, 1.0], 0.5),
            },
        ),
        envelope("aether.render", &output_overlay(output_id)),
        envelope("aether.render", &control_quad()),
    ];
    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))])
        .expect("capture must survive the dropped dispatch");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode surviving capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    assert!(pixel_is_lit(&img, 4, 4, bg, tolerance), "the control quad must draw — the frame's passes ran");
    let probe = rgba_at(&img, 32, 24);
    assert!(
        probe[0].abs_diff(bg[0]) <= tolerance
            && probe[1].abs_diff(bg[1]) <= tolerance
            && probe[2].abs_diff(bg[2]) <= tolerance,
        "the dropped dispatch must leave the output cleared, so the probe stays background; \
         bg={bg:?} probe={probe:?}",
    );
}
