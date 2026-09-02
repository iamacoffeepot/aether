//! Offscreen device-loss recovery scenario (ADR-0173, issue #4538).
//!
//! The concrete harness destroys generation zero without changing any actor
//! mail surface. A capture already parked and ready then drives the first
//! replacement frame. The scenario observes CPU-backed sampled pixels and
//! geometry after replacement, cleared GPU-only writable content before
//! redispatch, stable public ids, and one after-mail release.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::print_stderr)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, pixel_is_lit, rgba_at};
use aether_harness_substrate_capture::visual::{Image, background_top_left, decode_png};
use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DrawPass, DrawTexturedQuads,
    GeometrySlotSpec, OutputSlot, PassLoad, PassStage, ProgramDispatch, ProgramPass, ProgramRegister,
    ProgramRegisterResult, QuadBlend, SlotExtent, SlotSpec, TextureFormat, TextureSampling, TextureUsage, TexturedQuad,
    VertexAttribute, VertexFormat,
};

const DRAW_WGSL: &str = r"
struct DrawParams { color: vec4<f32> }
@group(0) @binding(0) var<uniform> draw_params: DrawParams;

@vertex
fn vs_main(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return draw_params.color;
}
";

fn require_wgpu() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

fn sampled_texture(pixels: Vec<u8>) -> CreateTexture {
    CreateTexture {
        width: 2,
        height: 2,
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Nearest,
        usage: TextureUsage::Sampled,
        pixels,
    }
}

fn writable_texture() -> CreateTexture {
    CreateTexture {
        width: 16,
        height: 16,
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Linear,
        usage: TextureUsage::Writable,
        pixels: Vec::new(),
    }
}

fn create_texture(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateTexture) -> u32 {
    match harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("create texture sequence")
        .reply::<CreateTextureResult>(label)
        .expect("decode create texture reply")
    {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create texture failed: {error}"),
    }
}

fn position_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
}

fn triangle_geometry() -> CreateGeometry {
    let positions = [[-0.8f32, -0.8, 0.0], [0.8, -0.8, 0.0], [0.0, 0.8, 0.0]];
    CreateGeometry {
        layout: position_layout(),
        vertices: positions.iter().flatten().flat_map(|value| value.to_le_bytes()).collect(),
        indices: [0u32, 1, 2].iter().flat_map(|index| index.to_le_bytes()).collect(),
    }
}

fn create_geometry(harness: &mut SubstrateHarness, label: &'static str) -> u32 {
    match harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", &triangle_geometry()))])
        .expect("create geometry sequence")
        .reply::<CreateGeometryResult>(label)
        .expect("decode create geometry reply")
    {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { reason } => panic!("create geometry failed: {reason}"),
    }
}

fn draw_program() -> ProgramRegister {
    ProgramRegister {
        wgsl: DRAW_WGSL.to_owned(),
        bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        transients: Vec::new(),
        geometries: vec![GeometrySlotSpec { layout: position_layout() }],
        depth_transients: Vec::new(),
        passes: vec![ProgramPass {
            stage: PassStage::Draw(DrawPass {
                vertex_entry_point: "vs_main".to_owned(),
                geometry: 0,
                depth: None,
                load: PassLoad::Clear,
            }),
            entry_point: "fs_main".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: 16,
            repeat: None,
        }],
    }
}

fn register_program(harness: &mut SubstrateHarness, label: &'static str) -> u32 {
    match harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", &draw_program()))])
        .expect("register program sequence")
        .reply::<ProgramRegisterResult>(label)
        .expect("decode register program reply")
    {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register program failed: {reason}"),
    }
}

fn dispatch(program_id: u32, output_id: u32, geometry_id: u32) -> ProgramDispatch {
    ProgramDispatch {
        program_id,
        bindings: vec![output_id],
        geometries: vec![geometry_id],
        uniforms: [1.0f32, 1.0, 1.0, 1.0].iter().flat_map(|value| value.to_le_bytes()).collect(),
    }
}

fn overlay(texture_id: u32, x: f32, y: f32, width: f32, height: f32) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        space: QuadSpace::Screen,
        clip: None,
        blend: QuadBlend::Straight,
        layer: 0,
        quads: vec![TexturedQuad {
            x,
            y,
            width,
            height,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

fn capture(
    harness: &mut SubstrateHarness,
    label: &'static str,
    pre: Vec<aether_kinds::NamedMail>,
    after: Vec<aether_kinds::NamedMail>,
) -> Image {
    let result = harness.execute(vec![(label, HarnessOp::capture_with_mails(pre, after))]).expect("capture sequence");
    decode_png(result.captured(label).expect("capture step ran")).expect("decode capture png")
}

#[test]
fn offscreen_loss_rebuilds_cpu_resources_clears_writable_state_and_keeps_ids() {
    if !require_wgpu() {
        return;
    }

    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot render harness");
    let sampled_id = create_texture(
        &mut harness,
        "sampled",
        &sampled_texture(vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]),
    );
    let output_id = create_texture(&mut harness, "output", &writable_texture());
    let geometry_id = create_geometry(&mut harness, "geometry");
    let program_id = register_program(&mut harness, "program");
    assert_eq!((sampled_id, output_id, geometry_id, program_id), (0, 1, 0, 0));

    let before = capture(
        &mut harness,
        "before",
        vec![
            envelope("aether.render", &dispatch(program_id, output_id, geometry_id)),
            envelope("aether.render", &overlay(output_id, 16.0, 8.0, 32.0, 32.0)),
        ],
        Vec::new(),
    );
    let background = background_top_left(&before);
    assert!(pixel_is_lit(&before, 32, 28, background, 5), "the pre-loss draw pass must paint its triangle");

    assert_eq!(harness.force_render_device_loss().expect("force generation zero loss"), 0);

    // This capture parks while generation zero is known lost. Its first frame
    // installs generation one, rebuilds the sampled texture from CPU pixels,
    // and realizes the GPU-only writable texture as a transparent clear. The
    // after-mail consumes exactly one texture id after readback.
    let control = capture(
        &mut harness,
        "control",
        vec![
            envelope("aether.render", &overlay(sampled_id, 2.0, 2.0, 8.0, 8.0)),
            envelope("aether.render", &overlay(output_id, 16.0, 8.0, 32.0, 32.0)),
        ],
        vec![envelope(
            "aether.render",
            &sampled_texture(vec![12, 34, 56, 255, 12, 34, 56, 255, 12, 34, 56, 255, 12, 34, 56, 255]),
        )],
    );
    let red = rgba_at(&control, 3, 3);
    assert!(red[0] > red[1] + 100 && red[0] > red[2] + 100, "sampled pixels must rebuild; got {red:?}");
    let cleared = rgba_at(&control, 32, 28);
    assert!(
        cleared[0].abs_diff(background[0]) <= 5
            && cleared[1].abs_diff(background[1]) <= 5
            && cleared[2].abs_diff(background[2]) <= 5,
        "writable content cannot be reconstructed and must restart cleared; bg={background:?}, got={cleared:?}",
    );

    let recovery = capture(
        &mut harness,
        "recovery",
        vec![
            envelope("aether.render", &dispatch(program_id, output_id, geometry_id)),
            envelope("aether.render", &overlay(output_id, 16.0, 8.0, 32.0, 32.0)),
        ],
        Vec::new(),
    );
    assert!(
        pixel_is_lit(&recovery, 32, 28, background, 5),
        "the same program, writable texture, and geometry ids must redispatch on the replacement device",
    );

    // Texture id 2 was consumed once by the control capture's after-mail. A
    // duplicate release would make this 4; no release would make it 2.
    assert_eq!(create_texture(&mut harness, "post_texture", &sampled_texture(vec![0; 16])), 3);
    assert_eq!(create_geometry(&mut harness, "post_geometry"), 1);
    assert_eq!(register_program(&mut harness, "post_program"), 1);
}
