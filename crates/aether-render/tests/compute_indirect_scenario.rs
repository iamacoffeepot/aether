//! Authored compute-to-indirect harness scenario (issue #4555): a
//! compute pass reads one resident geometry, writes another geometry's
//! resident vertex/index/control buffers, and a following indexed-
//! indirect stage rasterizes the derived triangle without CPU readback.

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
    ComputeBufferBinding, ComputePass, CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult,
    DrawPass, DrawTexturedQuads, GeometryBuffer, GeometrySlotSpec, OutputSlot, PassLoad, PassStage, PassStageKind,
    ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, ProgramTimings, ProgramTimingsResult,
    QuadBlend, SlotExtent, SlotSpec, StorageAccess, TextureFormat, TextureSampling, TextureUsage, TexturedQuad,
    UpdateGeometry, VertexAttribute, VertexFormat,
};

const MODULE: &str = r"
@group(2) @binding(0) var<storage, read> source_vertices: array<u32>;
@group(2) @binding(1) var<storage, read_write> output_vertices: array<u32>;
@group(2) @binding(2) var<storage, read_write> output_indices: array<u32>;
@group(2) @binding(3) var<storage, read_write> draw_control: array<u32>;

@compute @workgroup_size(1)
fn cs_derive() {
    for (var word = 0u; word < 9u; word += 1u) {
        output_vertices[word] = source_vertices[word];
    }
    output_indices[0] = 0u;
    output_indices[1] = 1u;
    output_indices[2] = 2u;
    draw_control[0] = 3u;
    draw_control[1] = 1u;
    draw_control[2] = 0u;
    draw_control[3] = 0u;
    draw_control[4] = 0u;
    draw_control[7] = 0u;
}

@vertex
fn vs_derived(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position, 1.0);
}

@fragment
fn fs_red() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}
";

const CENTERED: [[f32; 3]; 3] = [[-0.8, -0.8, 0.0], [0.8, -0.8, 0.0], [0.0, 0.8, 0.0]];
const SHIFTED_RIGHT: [[f32; 3]; 3] = [[0.1, -0.8, 0.0], [0.9, -0.8, 0.0], [0.5, 0.8, 0.0]];
const INDICES: [u32; 3] = [0, 1, 2];

fn require_wgpu() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

fn position_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
}

fn vertex_bytes(positions: &[[f32; 3]]) -> Vec<u8> {
    positions.iter().flatten().flat_map(|value| value.to_le_bytes()).collect()
}

fn index_bytes(indices: &[u32]) -> Vec<u8> {
    indices.iter().flat_map(|index| index.to_le_bytes()).collect()
}

fn create_geometry(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateGeometry) -> u32 {
    match harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("create geometry sequence")
        .reply::<CreateGeometryResult>(label)
        .expect("decode create geometry reply")
    {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { reason } => panic!("create geometry failed: {reason}"),
    }
}

fn create_output(harness: &mut SubstrateHarness) -> u32 {
    let mail = CreateTexture {
        width: 16,
        height: 16,
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Linear,
        usage: TextureUsage::Writable,
        pixels: Vec::new(),
    };
    match harness
        .execute(vec![("create_output", HarnessOp::send_and_await_reply("aether.render", &mail))])
        .expect("create output sequence")
        .reply::<CreateTextureResult>("create_output")
        .expect("decode create output reply")
    {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create output failed: {error}"),
    }
}

fn program() -> ProgramRegister {
    let slot = GeometrySlotSpec { layout: position_layout() };
    ProgramRegister {
        wgsl: MODULE.to_owned(),
        bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        transients: Vec::new(),
        geometries: vec![slot.clone(), slot],
        depth_transients: Vec::new(),
        passes: vec![
            ProgramPass {
                stage: PassStage::Compute(ComputePass {
                    buffers: vec![
                        ComputeBufferBinding {
                            geometry: 0,
                            buffer: GeometryBuffer::Vertices,
                            access: StorageAccess::Read,
                        },
                        ComputeBufferBinding {
                            geometry: 1,
                            buffer: GeometryBuffer::Vertices,
                            access: StorageAccess::ReadWrite,
                        },
                        ComputeBufferBinding {
                            geometry: 1,
                            buffer: GeometryBuffer::Indices,
                            access: StorageAccess::ReadWrite,
                        },
                        ComputeBufferBinding {
                            geometry: 1,
                            buffer: GeometryBuffer::DrawIndexedIndirect,
                            access: StorageAccess::ReadWrite,
                        },
                    ],
                    workgroups: [1, 1, 1],
                }),
                entry_point: "cs_derive".to_owned(),
                inputs: Vec::new(),
                output: OutputSlot::None,
                uniform_offset: 0,
                uniform_length: 0,
                repeat: None,
            },
            ProgramPass {
                stage: PassStage::DrawIndexedIndirect(DrawPass {
                    vertex_entry_point: "vs_derived".to_owned(),
                    geometry: 1,
                    depth: None,
                    load: PassLoad::Clear,
                }),
                entry_point: "fs_red".to_owned(),
                inputs: Vec::new(),
                output: OutputSlot::Binding { index: 0 },
                uniform_offset: 0,
                uniform_length: 0,
                repeat: None,
            },
        ],
    }
}

fn register_program(harness: &mut SubstrateHarness) -> u32 {
    match harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", &program()))])
        .expect("register sequence")
        .reply::<ProgramRegisterResult>("register")
        .expect("decode register reply")
    {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    }
}

fn dispatch(program_id: u32, output_id: u32, source_id: u32, derived_id: u32) -> ProgramDispatch {
    ProgramDispatch {
        program_id,
        bindings: vec![output_id],
        geometries: vec![source_id, derived_id],
        uniforms: Vec::new(),
    }
}

fn overlay(texture_id: u32) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        space: QuadSpace::Screen,
        clip: None,
        blend: QuadBlend::Straight,
        quads: vec![TexturedQuad {
            x: 16.0,
            y: 8.0,
            width: 32.0,
            height: 32.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

fn capture(harness: &mut SubstrateHarness, label: &'static str, pre: Vec<aether_kinds::NamedMail>) -> Image {
    let result =
        harness.execute(vec![(label, HarnessOp::capture_with_mails(pre, Vec::new()))]).expect("capture sequence");
    decode_png(result.captured(label).expect("capture step ran")).expect("decode capture png")
}

fn assert_red(image: &Image, x: u32, y: u32) {
    let pixel = rgba_at(image, x, y);
    assert!(pixel[0] > pixel[1] + 100 && pixel[0] > pixel[2] + 100, "expected a red derived pixel; got {pixel:?}");
}

#[test]
fn compute_derives_indirect_geometry_refreshes_after_update_and_recovers_after_device_loss() {
    if !require_wgpu() {
        return;
    }

    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render_pass_timings().build().expect("boot render harness");
    let source =
        CreateGeometry { layout: position_layout(), vertices: vertex_bytes(&CENTERED), indices: index_bytes(&INDICES) };
    let derived = CreateGeometry {
        layout: position_layout(),
        vertices: vertex_bytes(&[[0.0; 3]; 3]),
        indices: index_bytes(&[0, 0, 0]),
    };
    let source_id = create_geometry(&mut harness, "source", &source);
    let derived_id = create_geometry(&mut harness, "derived", &derived);
    let output_id = create_output(&mut harness);
    let program_id = register_program(&mut harness);

    let first = capture(
        &mut harness,
        "first",
        vec![
            envelope("aether.render", &dispatch(program_id, output_id, source_id, derived_id)),
            envelope("aether.render", &overlay(output_id)),
        ],
    );
    let background = background_top_left(&first);
    assert_red(&first, 32, 30);

    let shifted = capture(
        &mut harness,
        "shifted",
        vec![
            envelope(
                "aether.render",
                &UpdateGeometry {
                    geometry_id: source_id,
                    vertices: vertex_bytes(&SHIFTED_RIGHT),
                    indices: index_bytes(&INDICES),
                },
            ),
            envelope("aether.render", &dispatch(program_id, output_id, source_id, derived_id)),
            envelope("aether.render", &overlay(output_id)),
        ],
    );
    assert_red(&shifted, 40, 30);
    let old_center = rgba_at(&shifted, 28, 30);
    assert!(
        old_center[0].abs_diff(background[0]) <= 5
            && old_center[1].abs_diff(background[1]) <= 5
            && old_center[2].abs_diff(background[2]) <= 5,
        "updating the source must rebuild the storage bind group and move the derived triangle; got {old_center:?}",
    );

    harness.force_render_device_loss().expect("force render device loss");
    let recovered = capture(
        &mut harness,
        "recovered",
        vec![
            envelope("aether.render", &dispatch(program_id, output_id, source_id, derived_id)),
            envelope("aether.render", &overlay(output_id)),
        ],
    );
    assert!(pixel_is_lit(&recovered, 40, 30, background, 5), "replacement-device dispatch must draw");
    assert_red(&recovered, 40, 30);

    let dispatch = dispatch(program_id, output_id, source_id, derived_id);
    for _ in 0..6 {
        harness
            .execute(vec![
                ("dispatch", HarnessOp::send_and_settle("aether.render", &dispatch)),
                ("settle", HarnessOp::advance(2)),
            ])
            .expect("timed dispatch frame");
    }
    let timings = harness
        .execute(vec![("timings", HarnessOp::send_and_await_reply("aether.render", &ProgramTimings { program_id }))])
        .expect("timings sequence")
        .reply::<ProgramTimingsResult>("timings")
        .expect("decode timings reply");
    match timings {
        ProgramTimingsResult::Absent { reason } => {
            assert!(!reason.trim().is_empty(), "an unavailable timing instrument must say why");
        }
        ProgramTimingsResult::Err { reason } => panic!("timings for the registered program failed: {reason}"),
        ProgramTimingsResult::Ok { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].stage, PassStageKind::Compute);
            assert_eq!((rows[0].width, rows[0].height, rows[0].divisor), (0, 0, 1));
            assert!(rows[0].samples > 0, "the compute timestamp bracket must fold a sample: {rows:?}");
            assert_eq!(rows[1].stage, PassStageKind::Draw);
            assert!(rows[1].samples > 0, "the indirect draw timestamp bracket must fold a sample: {rows:?}");
        }
    }
}
