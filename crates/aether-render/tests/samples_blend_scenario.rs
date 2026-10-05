//! Sample-count and blend scenarios (ADR-0246 decision 7): a transient
//! that declares four samples per texel and is read resolved, and a
//! pass that declares how it composes with its output, driven
//! end-to-end through an in-process `SubstrateHarness`.
//!
//! Every scenario writes a `Target(Full)` output and observes it by
//! drawing it through the overlay path in the same captured frame; an
//! output texel that is transparent shows the frame's background.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

use aether_data::{Blob, Kind};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{
    envelope, pixel_is_lit, require_wgpu_adapter, rgb_close, rgba_at, srgb_byte_to_linear,
};
use aether_harness_substrate_capture::visual::{Image, background_top_left, decode_png};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::{
    Blend, CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DepthExtent, DepthSpec, DrawPass,
    DrawTexturedQuads, GeometrySlotSpec, InputSlot, Mips, OutputSlot, PassLoad, PassRepeat, PassStage, ProgramDispatch,
    ProgramPass, ProgramRegister, ProgramRegisterResult, QuadBlend, RenderCapability, Samples, Sampling, SlotExtent,
    SlotShape, SlotSpec, TextureFormat, TextureSampling, TextureUsage, TexturedQuad, TransientSpec, VertexAttribute,
    VertexFormat, Wrap,
};

/// Side of every program output texture and of the overlay quad that
/// reads it back, so one output texel is one frame pixel.
const OUTPUT_SIDE: u16 = 32;

/// Top edge of every readback quad in the 64x48 frame.
const QUAD_TOP: u16 = 8;

/// Output texels at the middle of the left and the right half.
const LEFT: (u32, u32) = (8, 16);
const RIGHT: (u32, u32) = (24, 16);

/// The output texel whose column the edge quad's right edge runs down
/// the middle of.
const EDGE: (u32, u32) = (16, 16);

/// Clip-space `x` of the middle of texel column 16 of a 32-texel
/// output: `-1 + 2 * 16.5 / 32`.
const EDGE_CLIP: f32 = 0.031_25;

const RED: [u8; 4] = [255, 0, 0, 255];

/// How far a captured channel may sit from the colour it shows.
const TOLERANCE: u8 = 30;

/// How far a captured channel's linear value may sit from the value it
/// shows. A quarter is the distance between the values the scenarios
/// tell apart, so this is well inside it.
const LINEAR_TOLERANCE: f32 = 0.08;

/// `fs_red` serves as the fragment stage of a draw pass and as a
/// fullscreen fragment pass. `fs_copy` hands a transient on as it is;
/// `fs_flatten` hands its colour on opaque, so what the frame shows is
/// the colour alone, whatever is behind it.
const MODULE: &str = r"
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@vertex
fn vs_flat(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position, 1.0);
}

@fragment
fn fs_red() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}

@fragment
fn fs_erase() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 0.0, 0.0, 0.0);
}

@fragment
fn fs_quarter() -> @location(0) vec4<f32> {
    return vec4<f32>(0.25, 0.25, 0.25, 0.25);
}

@fragment
fn fs_copy(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, uv);
}

@fragment
fn fs_flatten(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(source_texture, source_sampler, uv).rgb, 1.0);
}
";

fn position_slot() -> GeometrySlotSpec {
    GeometrySlotSpec { layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }] }
}

/// A full-extent `Rgba8` binding a pass may write.
fn target() -> SlotSpec {
    SlotSpec {
        format: TextureFormat::Rgba8,
        shape: SlotShape::Target(SlotExtent::Full),
        sampling: Sampling::Filtered { wrap: Wrap::Clamp, mips: Mips::Base },
    }
}

/// A fullscreen fragment pass with no inputs.
fn fragment(entry: &str, output: OutputSlot, blend: Blend) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        blend,
        entry_point: entry.to_owned(),
        inputs: Vec::new(),
        output,
        uniform_offset: 0,
        uniform_length: 0,
        repeat: None,
    }
}

/// A fullscreen pass through `entry` that reads transient 0 and
/// replaces `output` with what it returns.
fn copy(entry: &str, output: OutputSlot) -> ProgramPass {
    ProgramPass { inputs: vec![InputSlot::Transient { index: 0 }], ..fragment(entry, output, Blend::Replace) }
}

/// A draw of geometry slot `geometry` through `entry`, replacing what
/// it covers of `output`.
fn draw(entry: &str, geometry: u32, depth: Option<u32>, load: PassLoad, output: OutputSlot) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Draw(DrawPass { vertex_entry_point: "vs_flat".to_owned(), geometry, depth, load }),
        ..fragment(entry, output, Blend::Replace)
    }
}

/// Run one request and decode its reply as `R`. The request comes first
/// so a call site can build it from the harness it then lends.
fn reply<R: Kind>(request: HarnessOp, label: &'static str, harness: &mut SubstrateHarness) -> R {
    harness.execute(vec![(label, request)]).expect("request sequence").reply::<R>(label).expect("decode the reply")
}

/// A full-height quad between clip-space `left` and `right`, at depth
/// 0.5.
fn create_quad(harness: &mut SubstrateHarness, label: &'static str, left: f32, right: f32) -> u32 {
    let corners: [[f32; 3]; 4] = [[left, -1.0, 0.5], [right, -1.0, 0.5], [right, 1.0, 0.5], [left, 1.0, 0.5]];
    let indices: [u32; 6] = [0, 1, 2, 0, 2, 3];
    let mail = CreateGeometry {
        layout: position_slot().layout,
        vertices: Blob::from(corners.iter().flatten().flat_map(|value| value.to_le_bytes()).collect::<Vec<u8>>()),
        indices: Blob::from(indices.iter().flat_map(|index| index.to_le_bytes()).collect::<Vec<u8>>()),
    };
    let created: CreateGeometryResult =
        reply(HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail), label, harness);
    match created {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { error } => panic!("create_geometry ({label}) failed: {error}"),
    }
}

/// A writable `Rgba8` texture a program here writes.
fn create_output(harness: &mut SubstrateHarness, label: &'static str) -> u32 {
    let mail = CreateTexture {
        width: u32::from(OUTPUT_SIDE),
        height: u32::from(OUTPUT_SIDE),
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Nearest,
        usage: TextureUsage::Writable,
        pixels: Blob::from(Vec::new()),
    };
    let created: CreateTextureResult =
        reply(HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail), label, harness);
    match created {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture ({label}) failed: {error}"),
    }
}

fn register(harness: &mut SubstrateHarness, label: &'static str, mail: &ProgramRegister) -> u32 {
    let registered: ProgramRegisterResult =
        reply(HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), mail), label, harness);
    match registered {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { error } => panic!("register ({label}) failed: {error}"),
    }
}

fn dispatch(program_id: u32, bindings: Vec<u32>, geometries: Vec<u32>) -> ProgramDispatch {
    ProgramDispatch { program_id, bindings, geometries, draw_sets: Vec::new(), uniforms: Vec::new() }
}

/// An overlay draw of a program's output as an `OUTPUT_SIDE` square
/// whose left edge is at `left_edge` and whose top edge is at
/// `QUAD_TOP`.
fn output_overlay(texture_id: u32, left_edge: u16) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![TexturedQuad {
            x: f32::from(left_edge),
            y: f32::from(QUAD_TOP),
            width: f32::from(OUTPUT_SIDE),
            height: f32::from(OUTPUT_SIDE),
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

/// Run `dispatches` in order, then capture a frame showing each of
/// `outputs` through an overlay at its left edge.
fn capture(harness: &mut SubstrateHarness, dispatches: &[ProgramDispatch], outputs: &[(u32, u16)]) -> Image {
    let program_mails = dispatches.iter().map(|dispatch| envelope("aether.render", dispatch));
    let overlay_mails =
        outputs.iter().map(|&(output_id, left_edge)| envelope("aether.render", &output_overlay(output_id, left_edge)));
    let pre = program_mails.chain(overlay_mails).collect();
    let captured = harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture frame");
    decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png")
}

/// The frame pixel showing output texel `texel` of a quad at
/// `left_edge`.
fn frame_pixel(left_edge: u16, texel: (u32, u32)) -> (u32, u32) {
    (u32::from(left_edge) + texel.0, u32::from(QUAD_TOP) + texel.1)
}

fn shows_red(img: &Image, left_edge: u16, texel: (u32, u32)) -> bool {
    let (x, y) = frame_pixel(left_edge, texel);
    rgb_close(rgba_at(img, x, y), [RED[0], RED[1], RED[2]], TOLERANCE)
}

fn unlit(img: &Image, left_edge: u16, texel: (u32, u32)) -> bool {
    let (x, y) = frame_pixel(left_edge, texel);
    !pixel_is_lit(img, x, y, background_top_left(img), TOLERANCE)
}

/// The linear red a captured texel shows.
fn linear_red(img: &Image, left_edge: u16, texel: (u32, u32)) -> f32 {
    let (x, y) = frame_pixel(left_edge, texel);
    srgb_byte_to_linear(rgba_at(img, x, y)[0])
}

fn near(value: f32, expected: f32) -> bool {
    (value - expected).abs() <= LINEAR_TOLERANCE
}

/// A draw pass draws geometry 0 in red into transient 0 under depth
/// slot 0, both of `samples` samples per texel, and a fragment pass
/// hands the transient's colour to the output.
fn edge_program(samples: Samples) -> ProgramRegister {
    ProgramRegister {
        wgsl: MODULE.to_owned(),
        bindings: vec![target()],
        transients: vec![TransientSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full, samples }],
        geometries: vec![position_slot()],
        depth_transients: vec![DepthSpec { extent: DepthExtent::Output(SlotExtent::Full), samples }],
        passes: vec![
            draw("fs_red", 0, Some(0), PassLoad::Clear, OutputSlot::Transient { index: 0 }),
            copy("fs_flatten", OutputSlot::Binding { index: 0 }),
        ],
    }
}

/// A red quad's edge runs down the middle of texel column 16. Drawn
/// into a `Four` transient, that column reads half red: two of its four
/// samples are covered. Drawn into a `One` transient it reads red or
/// black, since its one sample is covered or it is not. The named bugs:
/// the sample count not reaching the pipeline or the textures, which
/// leaves the `Four` column red or black; and the reader bound to the
/// multisampled texture, or the depth texture of a `Four` slot created
/// single-sample, each of which wgpu refuses, leaving the output
/// unwritten.
#[test]
fn a_four_transient_reads_a_half_covered_texel_as_half() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let quad = create_quad(&mut harness, "create_quad", -1.0, EDGE_CLIP);
    let four_output = create_output(&mut harness, "create_four_output");
    let one_output = create_output(&mut harness, "create_one_output");
    let four_program = register(&mut harness, "register_four", &edge_program(Samples::Four));
    let one_program = register(&mut harness, "register_one", &edge_program(Samples::One));

    let (four_left, one_left) = (0, 32);
    let dispatches =
        [dispatch(four_program, vec![four_output], vec![quad]), dispatch(one_program, vec![one_output], vec![quad])];
    let img = capture(&mut harness, &dispatches, &[(four_output, four_left), (one_output, one_left)]);

    assert!(shows_red(&img, four_left, LEFT), "the Four program draws the quad where it covers whole texels");
    assert!(shows_red(&img, one_left, LEFT), "the One program draws the quad where it covers whole texels");
    let four_edge = linear_red(&img, four_left, EDGE);
    let one_edge = linear_red(&img, one_left, EDGE);
    assert!(near(four_edge, 0.5), "a half-covered texel of a Four transient resolves to half red: {four_edge}");
    assert!(near(one_edge, 0.0) || near(one_edge, 1.0), "a One transient has no partial coverage: {one_edge}");
}

/// A `Four` transient is drawn on its left half and copied to a first
/// binding, then drawn on its right half under `PassLoad::Load` and
/// copied to the output. The first binding shows the left half alone
/// and the output shows both. The named bugs: resolving only at the
/// graph's last writer, which leaves the first copy reading a texture
/// nothing resolved into; and the multisampled texture not keeping the
/// first writer's samples, which loses the left half from the output.
#[test]
fn a_four_transient_read_between_two_writers_resolves_before_each_read() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let left_quad = create_quad(&mut harness, "create_left_quad", -1.0, 0.0);
    let right_quad = create_quad(&mut harness, "create_right_quad", 0.0, 1.0);
    let first = create_output(&mut harness, "create_first");
    let output = create_output(&mut harness, "create_output");
    let scene = OutputSlot::Transient { index: 0 };
    let program = register(
        &mut harness,
        "register",
        &ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![target(), target()],
            transients: vec![TransientSpec {
                format: TextureFormat::Rgba8,
                extent: SlotExtent::Full,
                samples: Samples::Four,
            }],
            geometries: vec![position_slot(), position_slot()],
            depth_transients: Vec::new(),
            passes: vec![
                draw("fs_red", 0, None, PassLoad::Clear, scene),
                copy("fs_copy", OutputSlot::Binding { index: 0 }),
                draw("fs_red", 1, None, PassLoad::Load, scene),
                copy("fs_copy", OutputSlot::Binding { index: 1 }),
            ],
        },
    );

    let (first_left, output_left) = (0, 32);
    let dispatches = [dispatch(program, vec![first, output], vec![left_quad, right_quad])];
    let img = capture(&mut harness, &dispatches, &[(first, first_left), (output, output_left)]);

    assert!(shows_red(&img, first_left, LEFT), "the first read sees what the first writer drew");
    assert!(unlit(&img, first_left, RIGHT), "the first read is resolved before the second writer draws");
    assert!(shows_red(&img, output_left, LEFT), "the second writer loads the first writer's samples");
    assert!(shows_red(&img, output_left, RIGHT), "the second read sees what the second writer drew");
}

/// A fragment pass repeated twice writes a quarter grey into an
/// `Rgba16Float` transient under `Additive`, and the final pass hands
/// the transient's colour to the output. The output reads half grey.
/// The named bug: a float output replacing whatever the pass declares,
/// which leaves the second iteration's quarter.
#[test]
fn an_additive_pass_accumulates_on_a_float_transient() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let output = create_output(&mut harness, "create_output");
    let accumulate = ProgramPass {
        repeat: Some(PassRepeat { count: 2, uniform_stride: 0 }),
        ..fragment("fs_quarter", OutputSlot::Transient { index: 0 }, Blend::Additive)
    };
    let program = register(
        &mut harness,
        "register",
        &ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![target()],
            transients: vec![TransientSpec {
                format: TextureFormat::Rgba16Float,
                extent: SlotExtent::Full,
                samples: Samples::One,
            }],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![accumulate, copy("fs_flatten", OutputSlot::Binding { index: 0 })],
        },
    );

    let img = capture(&mut harness, &[dispatch(program, vec![output], Vec::new())], &[(output, 16)]);

    let grey = linear_red(&img, 16, LEFT);
    assert!(near(grey, 0.5), "two quarters added on a float transient read a half: {grey}");
}

/// A pass paints an `Rgba8` output opaque red, and a second pass draws
/// transparent black over its left half under `Replace`. The left half
/// shows the frame's background and the right half stays red. The named
/// bug: an `Rgba8` output alpha-blending whatever the pass declares,
/// under which a source of alpha zero leaves the red.
#[test]
fn a_replace_pass_erases_what_an_rgba8_output_held() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let left_quad = create_quad(&mut harness, "create_left_quad", -1.0, 0.0);
    let output = create_output(&mut harness, "create_output");
    let painted = OutputSlot::Binding { index: 0 };
    let program = register(
        &mut harness,
        "register",
        &ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![target()],
            transients: Vec::new(),
            geometries: vec![position_slot()],
            depth_transients: Vec::new(),
            passes: vec![
                fragment("fs_red", painted, Blend::Replace),
                draw("fs_erase", 0, None, PassLoad::Load, painted),
            ],
        },
    );

    let img = capture(&mut harness, &[dispatch(program, vec![output], vec![left_quad])], &[(output, 16)]);

    assert!(unlit(&img, 16, LEFT), "a Replace pass writing alpha zero erases the red it covers");
    assert!(shows_red(&img, 16, RIGHT), "the red the second pass does not cover stays");
}
