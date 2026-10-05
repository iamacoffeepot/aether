//! Binding shape and sampling scenarios (ADR-0246 decision 5): what a
//! program binding takes and how it is read, driven end-to-end through
//! an in-process `SubstrateHarness` — a `Texture` binding of its own
//! size, a binding's wrap reaching the sampler, a `Texel` table read by
//! a vertex stage, and a `TextureArray` binding that registers and
//! refuses a plain texture.
//!
//! Every pixel scenario observes a program's writable output texture by
//! drawing it through the overlay path in the same captured frame.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

use aether_data::Blob;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, pixel_is_lit, require_adapter, rgb_close, rgba_at};
use aether_harness_substrate_capture::visual::{Image, background_top_left, decode_png};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DrawPass, DrawShapes, DrawTexturedQuads,
    GeometrySlotSpec, InputSlot, Mips, OutputSlot, PassLoad, PassStage, ProgramDispatch, ProgramPass, ProgramRegister,
    ProgramRegisterResult, QuadBlend, RenderCapability, Sampling, Shape, SlotExtent, SlotShape, SlotSpec,
    TextureFormat, TextureSampling, TextureUsage, TexturedQuad, VertexAttribute, VertexFormat, Wrap,
};

/// Side of every program output texture and of the overlay quad that
/// reads it back, so one output texel is one frame pixel.
const OUTPUT_SIDE: u16 = 32;

/// Top edge of every readback quad in the 64x48 frame.
const QUAD_TOP: u16 = 8;

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// How far a captured channel may sit from the texel it shows.
const TOLERANCE: u8 = 30;

/// A fragment program sampling one input. `fs_stretch` maps the input
/// across the whole output; `fs_twice` reads it at twice the horizontal
/// coordinate, so the right half of the output asks for coordinates
/// past the texture's edge.
const SAMPLE_MODULE: &str = r"
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@fragment
fn fs_stretch(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, uv);
}

@fragment
fn fs_twice(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, vec2<f32>(uv.x * 2.0, uv.y));
}
";

/// A draw program whose vertex stage reads texel 2 of a table and whose
/// fragment stage multiplies it by a second, sampled input. Input 0 is
/// a `Texel` slot, so it has a texture at binding 0 and nothing at
/// binding 1; input 1 keeps bindings 2 and 3.
const TABLE_MODULE: &str = r"
@group(1) @binding(0) var table: texture_2d<f32>;
@group(1) @binding(2) var tint_texture: texture_2d<f32>;
@group(1) @binding(3) var tint_sampler: sampler;

struct Varyings {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_table(@location(0) position: vec3<f32>) -> Varyings {
    var out: Varyings;
    out.position = vec4<f32>(position.xy, 0.5, 1.0);
    out.color = textureLoad(table, vec2<i32>(2, 0), 0);
    return out;
}

@fragment
fn fs_tinted(in: Varyings) -> @location(0) vec4<f32> {
    return in.color * textureSample(tint_texture, tint_sampler, vec2<f32>(0.5, 0.5));
}
";

/// A fragment program whose one input is an array texture.
const ARRAY_MODULE: &str = r"
@group(1) @binding(0) var layers: texture_2d_array<f32>;
@group(1) @binding(1) var layers_sampler: sampler;

@fragment
fn fs_layer(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(layers, layers_sampler, uv, 0);
}
";

fn filtered(wrap: Wrap) -> Sampling {
    Sampling::Filtered { wrap, mips: Mips::Base }
}

fn slot(shape: SlotShape, sampling: Sampling) -> SlotSpec {
    SlotSpec { format: TextureFormat::Rgba8, shape, sampling }
}

/// The program's result slot: the full-extent target its final pass
/// writes.
fn output_slot() -> SlotSpec {
    slot(SlotShape::Target(SlotExtent::Full), filtered(Wrap::Clamp))
}

/// A one-pass fragment program: `source` at binding 0, read by `entry`
/// into the output at binding 1.
fn sampling_program(wgsl: &str, entry: &str, source: SlotSpec) -> ProgramRegister {
    ProgramRegister {
        wgsl: wgsl.to_owned(),
        bindings: vec![source, output_slot()],
        transients: Vec::new(),
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes: vec![ProgramPass {
            stage: PassStage::Fragment,
            entry_point: entry.to_owned(),
            inputs: vec![InputSlot::Binding { index: 0 }],
            output: OutputSlot::Binding { index: 1 },
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }],
    }
}

fn create_texture(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateTexture) -> u32 {
    let created = harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), mail))])
        .expect("create_texture sequence");
    match created.reply::<CreateTextureResult>(label).expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture ({label}) failed: {error}"),
    }
}

/// A nearest-sampled `Rgba8` texture holding exactly `texels`, row by
/// row, so a probe reads one texel's colour and never a blend of two.
fn create_texels(harness: &mut SubstrateHarness, label: &'static str, width: u32, texels: &[[u8; 4]]) -> u32 {
    let height = u32::try_from(texels.len()).expect("texel count fits u32") / width;
    create_texture(
        harness,
        label,
        &CreateTexture {
            width,
            height,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: Blob::from(texels.concat()),
        },
    )
}

/// The writable `Rgba8` texture a program here draws into.
fn create_output(harness: &mut SubstrateHarness, label: &'static str) -> u32 {
    create_texture(
        harness,
        label,
        &CreateTexture {
            width: u32::from(OUTPUT_SIDE),
            height: u32::from(OUTPUT_SIDE),
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Writable,
            pixels: Blob::from(Vec::new()),
        },
    )
}

/// The 3x5 texture the sampling scenarios bind, a size no extent of a
/// 32x32 output resolves to: a red, a green and a blue column, with the
/// bottom row white so the height is observed as well as the width.
fn create_stripes(harness: &mut SubstrateHarness) -> u32 {
    let mut texels = [[RED, GREEN, BLUE]; 5].concat();
    texels[12..].fill(WHITE);
    create_texels(harness, "create_stripes", 3, &texels)
}

fn register_reply(
    harness: &mut SubstrateHarness,
    label: &'static str,
    mail: &ProgramRegister,
) -> ProgramRegisterResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), mail))])
        .expect("register sequence")
        .reply::<ProgramRegisterResult>(label)
        .expect("decode ProgramRegisterResult")
}

fn registered_id(harness: &mut SubstrateHarness, label: &'static str, mail: &ProgramRegister) -> u32 {
    match register_reply(harness, label, mail) {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { error } => panic!("register ({label}) failed: {error}"),
    }
}

fn register_err(harness: &mut SubstrateHarness, label: &'static str, mail: &ProgramRegister) -> String {
    match register_reply(harness, label, mail) {
        ProgramRegisterResult::Err { error } => error,
        ProgramRegisterResult::Ok { program_id } => panic!("register ({label}) must reject; got program {program_id}"),
    }
}

/// An overlay draw of a program's output as an `OUTPUT_SIDE` square
/// whose left edge is at `left` and whose top edge is at `QUAD_TOP`.
fn output_overlay(texture_id: u32, left: u16) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![TexturedQuad {
            x: f32::from(left),
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

fn dispatch(program_id: u32, bindings: Vec<u32>, geometries: Vec<u32>) -> ProgramDispatch {
    ProgramDispatch { program_id, bindings, geometries, uniforms: Vec::new() }
}

/// The frame pixel showing output texel `(x, y)` of a quad at `left`.
fn output_texel(img: &Image, left: u16, x: u32, y: u32) -> [u8; 4] {
    rgba_at(img, u32::from(left) + x, u32::from(QUAD_TOP) + y)
}

fn shows(probe: [u8; 4], texel: [u8; 4]) -> bool {
    rgb_close(probe, [texel[0], texel[1], texel[2]], TOLERANCE)
}

/// A `Texture` binding takes a texture of its own size: a 3x5 texture
/// is sampled across a 32x32 output and every probe carries the colour
/// of the texel it falls in. The named bug: the size rule still applied
/// to a `Texture` binding, which drops the dispatch because 3x5 is not
/// the output's extent and leaves the output cleared, every probe at
/// the frame's background.
#[test]
fn a_texture_binding_takes_a_texture_of_its_own_size() {
    if !require_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let stripes_id = create_stripes(&mut harness);
    let output_id = create_output(&mut harness, "create_output");
    let program_id = registered_id(
        &mut harness,
        "register",
        &sampling_program(SAMPLE_MODULE, "fs_stretch", slot(SlotShape::Texture, filtered(Wrap::Clamp))),
    );

    let left = 16;
    let pre = vec![
        envelope("aether.render", &dispatch(program_id, vec![stripes_id, output_id], Vec::new())),
        envelope("aether.render", &output_overlay(output_id, left)),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture program output");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

    // Three columns across 32 texels put their centres near x = 5, 16
    // and 27; five rows put row 2 at y = 16 and the white row at y = 29.
    for (x, y, texel) in [(5, 16, RED), (16, 16, GREEN), (27, 16, BLUE), (16, 29, WHITE)] {
        let probe = output_texel(&img, left, x, y);
        assert!(shows(probe, texel), "output texel ({x}, {y}) must show source texel {texel:?}; got {probe:?}");
    }
}

/// The binding's wrap reaches the sampler: the same texture read at
/// twice the horizontal coordinate shows its left column again on the
/// right under `Wrap::Repeat`, and its right edge column there under
/// `Wrap::Clamp`. The named bug: the declared wrap never reaching the
/// sampler, so both programs clamp and both probes show the edge.
#[test]
fn a_bindings_wrap_reaches_the_sampler() {
    if !require_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let stripes_id = create_stripes(&mut harness);
    let repeat_output = create_output(&mut harness, "create_repeat_output");
    let clamp_output = create_output(&mut harness, "create_clamp_output");
    let repeat_program = registered_id(
        &mut harness,
        "register_repeat",
        &sampling_program(SAMPLE_MODULE, "fs_twice", slot(SlotShape::Texture, filtered(Wrap::Repeat))),
    );
    let clamp_program = registered_id(
        &mut harness,
        "register_clamp",
        &sampling_program(SAMPLE_MODULE, "fs_twice", slot(SlotShape::Texture, filtered(Wrap::Clamp))),
    );

    let (repeat_left, clamp_left) = (0, 32);
    let pre = vec![
        envelope("aether.render", &dispatch(repeat_program, vec![stripes_id, repeat_output], Vec::new())),
        envelope("aether.render", &dispatch(clamp_program, vec![stripes_id, clamp_output], Vec::new())),
        envelope("aether.render", &output_overlay(repeat_output, repeat_left)),
        envelope("aether.render", &output_overlay(clamp_output, clamp_left)),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture program outputs");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

    // Output x = 2 reads u = 0.16, inside the red column under either
    // wrap. Output x = 18 reads u = 1.16: the red column again when the
    // texture tiles, the blue edge column when it clamps.
    for (label, left) in [("repeat", repeat_left), ("clamp", clamp_left)] {
        let inside = output_texel(&img, left, 2, 16);
        assert!(shows(inside, RED), "{label}: a coordinate inside the texture reads its left column; got {inside:?}");
    }
    let repeated = output_texel(&img, repeat_left, 18, 16);
    assert!(shows(repeated, RED), "Wrap::Repeat must show the left column past the edge; got {repeated:?}");
    let clamped = output_texel(&img, clamp_left, 18, 16);
    assert!(shows(clamped, BLUE), "Wrap::Clamp must show the edge column past the edge; got {clamped:?}");
}

fn position_slot() -> GeometrySlotSpec {
    GeometrySlotSpec { layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }] }
}

/// Two triangles covering the whole of clip space.
fn create_cover(harness: &mut SubstrateHarness) -> u32 {
    let positions: [[f32; 3]; 4] = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0], [-1.0, 1.0, 0.0]];
    let indices: [u32; 6] = [0, 1, 2, 0, 2, 3];
    let mail = CreateGeometry {
        layout: position_slot().layout,
        vertices: Blob::from(positions.iter().flatten().flat_map(|value| value.to_le_bytes()).collect::<Vec<u8>>()),
        indices: Blob::from(indices.iter().flat_map(|index| index.to_le_bytes()).collect::<Vec<u8>>()),
    };
    let created = harness
        .execute(vec![(
            "create_cover",
            HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail),
        )])
        .expect("create_geometry sequence");
    match created.reply::<CreateGeometryResult>("create_cover").expect("decode CreateGeometryResult") {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { error } => panic!("create_geometry failed: {error}"),
    }
}

/// A `Texel` input is read by a draw pass's vertex stage, and the input
/// declared after it keeps its binding numbers: the vertex stage loads
/// texel 2 of a 4x1 table (yellow), the fragment stage samples a second
/// input (magenta), and the output is their product, red. The named
/// bugs: a read-only input invisible to the vertex stage, which fails
/// the register; a missing sampler shifting the next input down to
/// bindings 1 and 2, which fails the register against a module that
/// declares it at 2 and 3; and the table read through a sampler instead
/// of exactly, which blends texel 2 with its neighbours.
#[test]
fn a_vertex_stage_reads_a_texel_table_ahead_of_a_filtered_input() {
    if !require_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let table_id = create_texels(&mut harness, "create_table", 4, &[BLUE, GREEN, [255, 255, 0, 255], BLUE]);
    let tint_id = create_texels(&mut harness, "create_tint", 1, &[[255, 0, 255, 255]]);
    let output_id = create_output(&mut harness, "create_output");
    let geometry_id = create_cover(&mut harness);
    let program_id = registered_id(
        &mut harness,
        "register",
        &ProgramRegister {
            wgsl: TABLE_MODULE.to_owned(),
            bindings: vec![
                slot(SlotShape::Texture, Sampling::Texel),
                slot(SlotShape::Texture, filtered(Wrap::Clamp)),
                output_slot(),
            ],
            transients: Vec::new(),
            geometries: vec![position_slot()],
            depth_transients: Vec::new(),
            passes: vec![ProgramPass {
                stage: PassStage::Draw(DrawPass {
                    vertex_entry_point: "vs_table".to_owned(),
                    geometry: 0,
                    depth: None,
                    load: PassLoad::Clear,
                }),
                entry_point: "fs_tinted".to_owned(),
                inputs: vec![InputSlot::Binding { index: 0 }, InputSlot::Binding { index: 1 }],
                output: OutputSlot::Binding { index: 2 },
                uniform_offset: 0,
                uniform_length: 0,
                repeat: None,
            }],
        },
    );

    let left = 16;
    let pre = vec![
        envelope("aether.render", &dispatch(program_id, vec![table_id, tint_id, output_id], vec![geometry_id])),
        envelope("aether.render", &output_overlay(output_id, left)),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture draw output");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

    let probe = output_texel(&img, left, 16, 16);
    assert!(shows(probe, RED), "table texel 2 (yellow) times the tint (magenta) is red; got {probe:?}");
}

/// A small white square in the frame's top-left corner: proof that the
/// frame's own passes ran when a scenario expects a dispatch to drop.
fn control_quad() -> DrawShapes {
    DrawShapes {
        space: QuadSpace::Screen,
        clip: None,
        shapes: vec![Shape {
            x: 2.0,
            y: 2.0,
            width: 5.0,
            height: 5.0,
            corner_radius: 0.0,
            fill: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            stroke: None,
            shadow: None,
            texture: None,
        }],
    }
}

/// A `TextureArray` binding registers against an array-typed shader
/// input, the same module is refused when the binding is declared a
/// plain `Texture`, and a dispatch that binds a plain texture at the
/// array binding drops while the frame survives. The named bugs: the
/// input layout left two-dimensional whatever the shape says, which
/// refuses the first register and accepts the second; and a plain
/// texture's view reaching an array binding, a device validation error
/// that poisons the whole frame rather than dropping one dispatch.
#[test]
fn a_texture_array_binding_registers_and_refuses_a_plain_texture() {
    if !require_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let plain_declared = register_err(
        &mut harness,
        "register_plain",
        &sampling_program(ARRAY_MODULE, "fs_layer", slot(SlotShape::Texture, filtered(Wrap::Clamp))),
    );
    assert!(
        plain_declared.starts_with("pipeline creation failed"),
        "an array-typed input against a Texture binding is refused in the reply; got: {plain_declared}",
    );
    let program_id = registered_id(
        &mut harness,
        "register_array",
        &sampling_program(ARRAY_MODULE, "fs_layer", slot(SlotShape::TextureArray, filtered(Wrap::Clamp))),
    );

    let stripes_id = create_stripes(&mut harness);
    let output_id = create_output(&mut harness, "create_output");
    let left = 16;
    let pre = vec![
        envelope("aether.render", &dispatch(program_id, vec![stripes_id, output_id], Vec::new())),
        envelope("aether.render", &output_overlay(output_id, left)),
        envelope("aether.render", &control_quad()),
    ];
    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))])
        .expect("capture must survive the dropped dispatch");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode surviving capture png");
    let bg = background_top_left(&img);

    assert!(pixel_is_lit(&img, 4, 4, bg, 5), "the control quad must draw — the frame's passes ran");
    for (x, y) in [(5, 16), (16, 16), (27, 16)] {
        let probe = output_texel(&img, left, x, y);
        assert!(
            rgb_close(probe, bg, 5),
            "the dropped dispatch must leave the output cleared, so output texel ({x}, {y}) shows the background; \
             bg={bg:?} probe={probe:?}",
        );
    }
}
