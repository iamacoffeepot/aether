//! Texture array scenarios (ADR-0246 decision 6): an array created with
//! a fixed side and layer count, a layer written in place, and the array
//! bound at a `TextureArray` program binding, driven end-to-end through
//! an in-process `SubstrateHarness`.
//!
//! Every pixel scenario observes a program's writable output texture by
//! drawing it through the overlay path in the same captured frame.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

use aether_data::Blob;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{
    envelope, pixel_is_lit, require_wgpu_adapter, rgb_close, rgba_at,
};
use aether_harness_substrate_capture::visual::{Image, background_top_left, decode_png};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::{
    CreateTexture, CreateTextureArray, CreateTextureArrayResult, CreateTextureResult, DrawShapes, DrawTexturedQuads,
    InputSlot, Mips, OutputSlot, PassStage, ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult,
    QuadBlend, RenderCapability, Sampling, Shape, SlotExtent, SlotShape, SlotSpec, TextureFormat, TextureSampling,
    TextureUsage, TexturedQuad, Wrap, WriteTextureLayer,
};

/// Side of every program output texture and of the overlay quad that
/// reads it back, so one output texel is one frame pixel.
const OUTPUT_SIDE: u16 = 32;

/// Top edge of every readback quad in the frame.
const QUAD_TOP: u16 = 8;

/// The frame: wide enough for three readback quads side by side.
const FRAME: (u32, u32) = (128, 48);

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

/// How far a captured channel may sit from the texel it shows.
const TOLERANCE: u8 = 30;

/// A fragment program over one array input. `fs_layer` reads the layer
/// its uniform window names; `fs_level_one` reads mip level 1 of layer 0.
const ARRAY_MODULE: &str = r"
struct Pick { layer: u32 }
@group(0) @binding(0) var<uniform> pick: Pick;
@group(1) @binding(0) var layers: texture_2d_array<f32>;
@group(1) @binding(1) var layers_sampler: sampler;

@fragment
fn fs_layer(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(layers, layers_sampler, uv, pick.layer);
}

@fragment
fn fs_level_one(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSampleLevel(layers, layers_sampler, uv, 0, 1.0);
}
";

/// A fragment program over one plain input.
const PLAIN_MODULE: &str = r"
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@fragment
fn fs_copy(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, uv);
}
";

fn boot() -> SubstrateHarness {
    SubstrateHarness::builder().size(FRAME.0, FRAME.1).with_render().build().expect("boot")
}

/// A one-pass fragment program: a source of `shape` at binding 0, read
/// by `entry` through `uniform_length` bytes of uniform window into the
/// full-extent output at binding 1.
fn program(wgsl: &str, entry: &str, shape: SlotShape, mips: Mips, uniform_length: u32) -> ProgramRegister {
    let sampling = Sampling::Filtered { wrap: Wrap::Clamp, mips };
    ProgramRegister {
        wgsl: wgsl.to_owned(),
        bindings: vec![
            SlotSpec { format: TextureFormat::Rgba8, shape, sampling },
            SlotSpec { format: TextureFormat::Rgba8, shape: SlotShape::Target(SlotExtent::Full), sampling },
        ],
        transients: Vec::new(),
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes: vec![ProgramPass {
            stage: PassStage::Fragment,
            entry_point: entry.to_owned(),
            inputs: vec![InputSlot::Binding { index: 0 }],
            output: OutputSlot::Binding { index: 1 },
            uniform_offset: 0,
            uniform_length,
            repeat: None,
        }],
    }
}

fn register(harness: &mut SubstrateHarness, mail: &ProgramRegister) -> u32 {
    let registered = harness
        .execute(vec![("register", HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), mail))])
        .expect("register sequence");
    match registered.reply::<ProgramRegisterResult>("register").expect("decode ProgramRegisterResult") {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { error } => panic!("register failed: {error}"),
    }
}

fn create_array(harness: &mut SubstrateHarness, side: u32, layers: u32, mips: Mips) -> u32 {
    let mail = CreateTextureArray { format: TextureFormat::Rgba8, side, layers, mips };
    let created = harness
        .execute(vec![(
            "create_array",
            HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail),
        )])
        .expect("create_texture_array sequence");
    match created.reply::<CreateTextureArrayResult>("create_array").expect("decode CreateTextureArrayResult") {
        CreateTextureArrayResult::Ok { texture_id } => texture_id,
        CreateTextureArrayResult::Err { error } => panic!("create_texture_array failed: {error}"),
    }
}

/// The writable `Rgba8` texture a program here draws into.
fn create_output(harness: &mut SubstrateHarness) -> u32 {
    let mail = CreateTexture {
        width: u32::from(OUTPUT_SIDE),
        height: u32::from(OUTPUT_SIDE),
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Nearest,
        usage: TextureUsage::Writable,
        pixels: Blob::from(Vec::new()),
    };
    let created = harness
        .execute(vec![(
            "create_output",
            HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail),
        )])
        .expect("create_texture sequence");
    match created.reply::<CreateTextureResult>("create_output").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    }
}

/// Write one layer and wait for the render actor to have staged it.
fn write_layer(harness: &mut SubstrateHarness, texture_id: u32, layer: u32, pixels: Vec<u8>) {
    let mail = WriteTextureLayer { texture_id, layer, pixels: Blob::from(pixels) };
    harness
        .execute(vec![("write_layer", HarnessOp::send_and_settle(&harness.actor_ref::<RenderCapability>(), &mail))])
        .expect("write_texture_layer sequence");
}

/// `texels` texels of one colour.
fn solid(colour: [u8; 4], texels: usize) -> Vec<u8> {
    colour.repeat(texels)
}

fn dispatch(program_id: u32, source_id: u32, output_id: u32, layer: Option<u32>) -> ProgramDispatch {
    let uniforms = layer.map(|layer| layer.to_le_bytes().to_vec()).unwrap_or_default();
    ProgramDispatch {
        program_id,
        bindings: vec![source_id, output_id],
        geometries: Vec::new(),
        draw_sets: Vec::new(),
        uniforms,
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

/// Dispatch each `(program, source, output, layer)` and capture a frame
/// showing every output, the first at the frame's left edge and each
/// next one `OUTPUT_SIDE` further right, with `extra` drawn after them.
fn capture(harness: &mut SubstrateHarness, dispatches: &[ProgramDispatch], extra: Option<&DrawShapes>) -> Image {
    let mut pre = Vec::new();
    for (index, mail) in dispatches.iter().enumerate() {
        let left = OUTPUT_SIDE * u16::try_from(index).expect("dispatch index fits u16");
        pre.push(envelope("aether.render", mail));
        pre.push(envelope("aether.render", &output_overlay(mail.bindings[1], left)));
    }
    pre.extend(extra.map(|shapes| envelope("aether.render", shapes)));

    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))])
        .expect("the capture must survive whatever the dispatches did");
    decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png")
}

/// The frame pixel at the centre of readback quad `index`.
fn output_centre(img: &Image, index: u32) -> [u8; 4] {
    let half = u32::from(OUTPUT_SIDE) / 2;
    rgba_at(img, index * u32::from(OUTPUT_SIDE) + half, u32::from(QUAD_TOP) + half)
}

fn shows(probe: [u8; 4], texel: [u8; 4]) -> bool {
    rgb_close(probe, [texel[0], texel[1], texel[2]], TOLERANCE)
}

/// A program reads the layer it asks for: layer 0 was written red and
/// layer 2 green, and layer 1 was never written, so it reads as zero and
/// its output shows the frame's background through a transparent quad.
/// The named bugs: a write that ignores its layer and lands every upload
/// in layer 0, which shows green for layer 0 and nothing for layer 2;
/// and an array bound through a two-dimensional view, a device
/// validation error that drops all three dispatches.
#[test]
fn a_program_reads_the_layer_it_asks_for() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = boot();
    let array_id = create_array(&mut harness, 2, 3, Mips::Base);
    write_layer(&mut harness, array_id, 0, solid(RED, 4));
    write_layer(&mut harness, array_id, 2, solid(GREEN, 4));
    let program_id = register(&mut harness, &program(ARRAY_MODULE, "fs_layer", SlotShape::TextureArray, Mips::Base, 4));
    let outputs = [create_output(&mut harness), create_output(&mut harness), create_output(&mut harness)];

    let dispatches = [0, 1, 2].map(|layer| dispatch(program_id, array_id, outputs[layer as usize], Some(layer)));
    let img = capture(&mut harness, &dispatches, None);
    let bg = background_top_left(&img);

    let (first, second, third) = (output_centre(&img, 0), output_centre(&img, 1), output_centre(&img, 2));
    assert!(shows(first, RED), "layer 0 was written red; got {first:?}");
    assert!(rgb_close(second, bg, 5), "layer 1 was never written and reads as zero; bg={bg:?} got {second:?}");
    assert!(shows(third, GREEN), "layer 2 was written green; got {third:?}");
}

/// A layer written again after the array was realized shows its new
/// contents. The named bug: a realized array whose later write is staged
/// and never uploaded, which leaves the second capture red.
#[test]
fn a_layer_written_after_realization_is_uploaded() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = boot();
    let array_id = create_array(&mut harness, 2, 3, Mips::Base);
    write_layer(&mut harness, array_id, 0, solid(RED, 4));
    let program_id = register(&mut harness, &program(ARRAY_MODULE, "fs_layer", SlotShape::TextureArray, Mips::Base, 4));
    let output_id = create_output(&mut harness);
    let read_layer_zero = [dispatch(program_id, array_id, output_id, Some(0))];

    let before = output_centre(&capture(&mut harness, &read_layer_zero, None), 0);
    assert!(shows(before, RED), "precondition: the first capture realizes the array and shows red; got {before:?}");

    write_layer(&mut harness, array_id, 0, solid(BLUE, 4));
    let after = output_centre(&capture(&mut harness, &read_layer_zero, None), 0);
    assert!(shows(after, BLUE), "the rewritten layer must reach the realized texture; got {after:?}");
}

/// A `Mips::Chain` array holds every level its write supplied: a
/// one-layer array of side 4 is written red, green and blue at levels 0,
/// 1 and 2, and a program reading level 1 shows green. The named bugs:
/// an upload of the base level only, which reads level 1 as zero; a
/// wrong level offset into the blob, which reads red or blue; and a
/// one-layer array bound through its default view, which is
/// two-dimensional and drops the dispatch.
#[test]
fn a_chain_array_holds_every_level_it_was_written() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = boot();
    let array_id = create_array(&mut harness, 4, 1, Mips::Chain);
    write_layer(&mut harness, array_id, 0, [solid(RED, 16), solid(GREEN, 4), solid(BLUE, 1)].concat());
    let program_id =
        register(&mut harness, &program(ARRAY_MODULE, "fs_level_one", SlotShape::TextureArray, Mips::Chain, 0));
    let output_id = create_output(&mut harness);

    let img = capture(&mut harness, &[dispatch(program_id, array_id, output_id, None)], None);

    let probe = output_centre(&img, 0);
    assert!(shows(probe, GREEN), "level 1 was written green; got {probe:?}");
}

/// A small white square in the frame's top-right corner, clear of every
/// readback quad: proof that the frame's own passes ran when a scenario
/// expects a dispatch to drop.
fn control_quad() -> DrawShapes {
    DrawShapes {
        space: QuadSpace::Screen,
        clip: None,
        shapes: vec![Shape {
            x: 118.0,
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

/// An array bound where the program declared a plain `Texture` drops the
/// dispatch and nothing else: the control quad still draws and the
/// output stays cleared. The named bug: the dispatch check indexing the
/// plain-texture map with an array id, which panics the render actor.
#[test]
fn an_array_at_a_plain_texture_binding_drops_the_dispatch() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = boot();
    let array_id = create_array(&mut harness, 2, 1, Mips::Base);
    write_layer(&mut harness, array_id, 0, solid(RED, 4));
    let program_id = register(&mut harness, &program(PLAIN_MODULE, "fs_copy", SlotShape::Texture, Mips::Base, 0));
    let output_id = create_output(&mut harness);

    let img = capture(&mut harness, &[dispatch(program_id, array_id, output_id, None)], Some(&control_quad()));
    let bg = background_top_left(&img);

    assert!(pixel_is_lit(&img, 120, 4, bg, 5), "the control quad must draw — the frame's passes ran");
    let probe = output_centre(&img, 0);
    assert!(rgb_close(probe, bg, 5), "the dropped dispatch must leave the output cleared; bg={bg:?} got {probe:?}");
}
