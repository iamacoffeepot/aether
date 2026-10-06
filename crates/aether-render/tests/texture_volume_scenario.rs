//! Volume texture scenarios (ADR-0246 decision 6): a volume created
//! whole, bound at a `TextureVolume` program binding and sampled at a
//! three-component coordinate, driven end-to-end through an in-process
//! `SubstrateHarness`.
//!
//! Every pixel scenario observes a program's writable output texture by
//! drawing it through the overlay path in the same captured frame.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

use std::ops::RangeInclusive;

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
    Blend, CreateTexture, CreateTextureResult, CreateTextureVolume, CreateTextureVolumeResult, DrawShapes,
    DrawTexturedQuads, InputSlot, Mips, OutputSlot, PassStage, ProgramDispatch, ProgramPass, ProgramRegister,
    ProgramRegisterResult, QuadBlend, RenderCapability, Sampling, Shape, SlotExtent, SlotShape, SlotSpec,
    TextureFormat, TextureSampling, TextureUsage, TexturedQuad, Wrap,
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

/// The captured range of a channel that two slices, one holding it at
/// zero and one at full, each contributed half of. It is wide enough for
/// the half to be stored linear or encoded, and excludes both ends: a
/// read that took one slice alone lands outside it.
const BLENDED: RangeInclusive<u8> = 64..=224;

/// The side of each test volume, on all three axes.
const VOLUME_SIDE: u32 = 2;

/// Texels in one slice of a test volume.
const SLICE_TEXELS: usize = (VOLUME_SIDE * VOLUME_SIDE) as usize;

/// A fragment program over one volume input, read at the third
/// coordinate its uniform window names. `fs_colour` writes what it
/// sampled; `fs_value` writes the sampled red channel and its complement
/// into two channels, so a one-channel volume shows both ends.
const VOLUME_MODULE: &str = r"
struct Pick { w: f32 }
@group(0) @binding(0) var<uniform> pick: Pick;
@group(1) @binding(0) var volume: texture_3d<f32>;
@group(1) @binding(1) var volume_sampler: sampler;

@fragment
fn fs_colour(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(volume, volume_sampler, vec3<f32>(uv, pick.w));
}

@fragment
fn fs_value(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let value = textureSample(volume, volume_sampler, vec3<f32>(uv, pick.w)).r;
    return vec4<f32>(value, 1.0 - value, 0.0, 1.0);
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

/// A filtered source slot of `shape` in `format`, addressed by `wrap`.
fn source(format: TextureFormat, shape: SlotShape, wrap: Wrap) -> SlotSpec {
    SlotSpec { format, shape, sampling: Sampling::Filtered { wrap, mips: Mips::Base } }
}

/// A one-pass fragment program: `source` at binding 0, read by `entry`
/// through `uniform_length` bytes of uniform window into the full-extent
/// `Rgba8` output at binding 1.
fn program(wgsl: &str, entry: &str, source: SlotSpec, uniform_length: u32) -> ProgramRegister {
    let output = SlotSpec {
        format: TextureFormat::Rgba8,
        shape: SlotShape::Target(SlotExtent::Full),
        sampling: Sampling::Filtered { wrap: Wrap::Clamp, mips: Mips::Base },
    };
    ProgramRegister {
        wgsl: wgsl.to_owned(),
        bindings: vec![source, output],
        transients: Vec::new(),
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes: vec![ProgramPass {
            stage: PassStage::Fragment,
            blend: Blend::Alpha,
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

/// A `VOLUME_SIDE` cube of two slices, `slices` being its whole contents.
fn create_volume(harness: &mut SubstrateHarness, format: TextureFormat, slices: &[Vec<u8>; 2]) -> u32 {
    let mail = CreateTextureVolume {
        format,
        width: VOLUME_SIDE,
        height: VOLUME_SIDE,
        depth: VOLUME_SIDE,
        pixels: Blob::from(slices.concat()),
    };
    let created = harness
        .execute(vec![(
            "create_volume",
            HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail),
        )])
        .expect("create_texture_volume sequence");
    match created.reply::<CreateTextureVolumeResult>("create_volume").expect("decode CreateTextureVolumeResult") {
        CreateTextureVolumeResult::Ok { texture_id } => texture_id,
        CreateTextureVolumeResult::Err { error } => panic!("create_texture_volume failed: {error}"),
    }
}

/// A plain texture of `side` texels square: `pixels` sampled, or a
/// writable render target when there are none.
fn create_plain(harness: &mut SubstrateHarness, side: u32, pixels: Vec<u8>) -> u32 {
    let usage = if pixels.is_empty() {
        TextureUsage::Writable
    } else {
        TextureUsage::Sampled
    };
    let mail = CreateTexture {
        width: side,
        height: side,
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Nearest,
        usage,
        pixels: Blob::from(pixels),
    };
    let created = harness
        .execute(vec![("create", HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail))])
        .expect("create_texture sequence");
    match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    }
}

/// The writable `Rgba8` texture a program here draws into.
fn create_output(harness: &mut SubstrateHarness) -> u32 {
    create_plain(harness, u32::from(OUTPUT_SIDE), Vec::new())
}

/// One slice of a test volume in one `Rgba8` colour.
fn colour_slice(colour: [u8; 4]) -> Vec<u8> {
    colour.repeat(SLICE_TEXELS)
}

/// One slice of a test volume holding one `R16Float` value, given as its
/// half-float bit pattern.
fn half_slice(bits: u16) -> Vec<u8> {
    bits.to_le_bytes().repeat(SLICE_TEXELS)
}

/// A dispatch reading `source_id` at third coordinate `w`, or with no
/// uniform window for a program that declares none.
fn dispatch(program_id: u32, source_id: u32, output_id: u32, w: Option<f32>) -> ProgramDispatch {
    let uniforms = w.map(|w| w.to_le_bytes().to_vec()).unwrap_or_default();
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

/// Dispatch each program and capture a frame showing every output, the
/// first at the frame's left edge and each next one `OUTPUT_SIDE`
/// further right, with `extra` drawn after them.
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

/// A program reads the slice its third coordinate names, and between two
/// slice centres the blend of both. Slice 0 of an `R16Float` volume
/// holds 0.0 and slice 1 holds 1.0; the shader writes the value to red
/// and its complement to green, so slice 0 shows green and slice 1 red.
/// The named bugs: slices uploaded in the wrong order, which swaps the
/// two ends; only the first image uploaded, which reads slice 1 as zero
/// and shows green at both ends; a two-byte texel uploaded at a
/// one-byte row stride or a volume bound through a two-dimensional view,
/// each a device validation error that drops every dispatch; and a
/// nearest sampler on a filterable volume, which shows one slice at the
/// midpoint.
#[test]
fn a_program_reads_the_slice_a_coordinate_names_and_the_blend_between_two() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = boot();
    let (zero, one) = (0x0000, 0x3C00);
    let volume_id = create_volume(&mut harness, TextureFormat::R16Float, &[half_slice(zero), half_slice(one)]);
    let slot = source(TextureFormat::R16Float, SlotShape::TextureVolume, Wrap::Clamp);
    let program_id = register(&mut harness, &program(VOLUME_MODULE, "fs_value", slot, 4));
    let outputs = [create_output(&mut harness), create_output(&mut harness), create_output(&mut harness)];

    let coordinates = [0.25, 0.75, 0.5];
    let dispatches = [0, 1, 2].map(|index| dispatch(program_id, volume_id, outputs[index], Some(coordinates[index])));
    let img = capture(&mut harness, &dispatches, None);

    let (near, far, between) = (output_centre(&img, 0), output_centre(&img, 1), output_centre(&img, 2));
    assert!(shows(near, GREEN), "w = 0.25 is the centre of slice 0, which holds 0.0; got {near:?}");
    assert!(shows(far, RED), "w = 0.75 is the centre of slice 1, which holds 1.0; got {far:?}");
    let value_blended = BLENDED.contains(&between[0]);
    let complement_blended = BLENDED.contains(&between[1]);
    assert!(
        value_blended && complement_blended,
        "w = 0.5 is halfway between the two slice centres and must blend them; got {between:?}",
    );
}

/// The binding's wrap reaches the third axis. Slice 0 is red and slice 1
/// blue, and two programs that differ only in `wrap` read at `w = 0.0`,
/// the near face of slice 0: half a texel before its centre. `Clamp`
/// extends slice 0 outward and shows red alone; `Repeat` finds the last
/// slice there and shows the blend of blue and red. The named bug: a
/// sampler whose third-axis address mode is left at the clamp default,
/// which makes a repeating volume snap at its seam and shows red twice.
#[test]
fn the_wrap_of_a_binding_reaches_the_third_axis() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = boot();
    let volume_id = create_volume(&mut harness, TextureFormat::Rgba8, &[colour_slice(RED), colour_slice(BLUE)]);
    let clamped = source(TextureFormat::Rgba8, SlotShape::TextureVolume, Wrap::Clamp);
    let repeated = source(TextureFormat::Rgba8, SlotShape::TextureVolume, Wrap::Repeat);
    let clamp_id = register(&mut harness, &program(VOLUME_MODULE, "fs_colour", clamped, 4));
    let repeat_id = register(&mut harness, &program(VOLUME_MODULE, "fs_colour", repeated, 4));
    let outputs = [create_output(&mut harness), create_output(&mut harness)];

    let dispatches =
        [dispatch(clamp_id, volume_id, outputs[0], Some(0.0)), dispatch(repeat_id, volume_id, outputs[1], Some(0.0))];
    let img = capture(&mut harness, &dispatches, None);

    let (clamp, repeat) = (output_centre(&img, 0), output_centre(&img, 1));
    assert!(shows(clamp, RED), "a clamped read at the near face stays in slice 0; got {clamp:?}");
    let red_blended = BLENDED.contains(&repeat[0]);
    let blue_blended = BLENDED.contains(&repeat[2]);
    assert!(
        red_blended && blue_blended,
        "a repeating read at the near face blends the last slice with the first; got {repeat:?}",
    );
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

/// The kind of texture has to match the binding's shape, in both
/// directions: a volume bound where the program declared a plain
/// `Texture`, and a plain texture bound where it declared a
/// `TextureVolume`, each drop their dispatch and nothing else. Every
/// texture here is `Rgba8`, so the format check cannot be what drops
/// them. The named bugs: a mismatched view reaching the bind group, a
/// device validation error that takes the frame's other work with it;
/// and the dispatch check indexing the plain-texture map with a volume
/// id, which panics the render actor.
#[test]
fn a_texture_of_the_wrong_kind_drops_the_dispatch() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = boot();
    let volume_id = create_volume(&mut harness, TextureFormat::Rgba8, &[colour_slice(RED), colour_slice(RED)]);
    let plain_id = create_plain(&mut harness, VOLUME_SIDE, colour_slice(GREEN));
    let plain_slot = source(TextureFormat::Rgba8, SlotShape::Texture, Wrap::Clamp);
    let volume_slot = source(TextureFormat::Rgba8, SlotShape::TextureVolume, Wrap::Clamp);
    let plain_program = register(&mut harness, &program(PLAIN_MODULE, "fs_copy", plain_slot, 0));
    let volume_program = register(&mut harness, &program(VOLUME_MODULE, "fs_colour", volume_slot, 4));
    let outputs = [create_output(&mut harness), create_output(&mut harness)];

    let dispatches = [
        dispatch(plain_program, volume_id, outputs[0], None),
        dispatch(volume_program, plain_id, outputs[1], Some(0.25)),
    ];
    let img = capture(&mut harness, &dispatches, Some(&control_quad()));
    let bg = background_top_left(&img);

    assert!(pixel_is_lit(&img, 120, 4, bg, 5), "the control quad must draw — the frame's passes ran");
    let (volume_at_plain, plain_at_volume) = (output_centre(&img, 0), output_centre(&img, 1));
    assert!(
        rgb_close(volume_at_plain, bg, 5),
        "a volume at a Texture binding must leave the output cleared; bg={bg:?} got {volume_at_plain:?}",
    );
    assert!(
        rgb_close(plain_at_volume, bg, 5),
        "a plain texture at a TextureVolume binding must leave the output cleared; bg={bg:?} got {plain_at_volume:?}",
    );
}
