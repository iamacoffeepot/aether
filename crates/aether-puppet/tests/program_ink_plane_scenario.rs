//! The ink coverage plane the wash reads, held against the reduction it
//! claims to be (iamacoffeepot/aether#4451, ADR-0172).
//!
//! `fs_ink_plane` takes the greatest alpha over each
//! [`stroke::INK_PLANE_FOOTPRINT`]-square block of the ink layer's own
//! raster and writes it as the wash body's coverage. The oracle here is
//! that sentence in Rust over the same raster bytes, so the budget is
//! zero: both answers are a maximum over the same sixteen numbers, and
//! either they agree texel for texel or the footprint arithmetic is
//! wrong.
//!
//! What the two cases are for. The first is the reduction itself — get
//! the block origin or the stride wrong and the plane samples the wrong
//! part of the drawing, which renders as a wash whose flow runs along
//! lines that are not there. The second is the property the whole plane
//! exists for: most of the drawing is thinner than one wash body texel,
//! so a stroke has to survive the reduction as a *continuous* line. A
//! mean, or one tap per block, finds it only where it happens to pass
//! near a sample — and a structure tensor over a dashed lock still
//! answers with a confident orientation, just the wrong one, so nothing
//! downstream reports the loss.

// Skip diagnostics go to stderr so `cargo test -- --nocapture` surfaces
// them next to the test name.
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
// The fixture's world coordinates read as "a fifth of a texel down from
// the rail", which is the geometry the case is about; `mul_add` would
// hide it.
#![allow(clippy::suboptimal_flops)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{
    append_capture_probe, envelope, has_wgpu_adapter, rgba_at, srgb_byte_to_linear,
};
use aether_harness_substrate_capture::visual::decode_png;
use aether_kinds::QuadSpace;
use aether_math::{Mat4, Rgb, Rgba, Vec3};
use aether_puppet::easel::program::{self, stroke};
use aether_puppet::easel::regions;
use aether_puppet::{deform, ribbon};
use aether_render::QuadBlend;
use aether_render::{
    CreateTexture, CreateTextureResult, DrawTexturedQuads, DrawTriangle, InputSlot, OutputSlot, PassStage,
    ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, SlotExtent, SlotSpec, TextureFormat,
    TextureSampling, TextureUsage, TexturedQuad, Vertex,
};

/// Raster texels per plane texel on each axis, as an index type.
const FOOTPRINT: usize = stroke::INK_PLANE_FOOTPRINT as usize;

/// The wash body extent the plane develops at. Small enough to compare
/// every texel by hand, large enough that a sliver spans a useful run of
/// them.
const PLANE_WIDTH: usize = 96;
const PLANE_HEIGHT: usize = 64;

/// The ink layer's own raster, which the plane is a reduction of.
const RASTER_WIDTH: usize = PLANE_WIDTH * FOOTPRINT;
const RASTER_HEIGHT: usize = PLANE_HEIGHT * FOOTPRINT;

fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// An orthographic camera over the unit square, so a world coordinate in
/// `[-0.5, 0.5]` lands on the page at a known texel and the fixtures can
/// be written in page-sized terms.
fn camera() -> Mat4 {
    Mat4::orthographic_rh(-0.5, 0.5, -0.5, 0.5, 1.0, 10.0)
        * Mat4::look_at_rh(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y)
}

fn triangle(a: Vec3, b: Vec3, c: Vec3) -> DrawTriangle {
    let vertex = |p: Vec3| Vertex { x: p.x, y: p.y, z: p.z, color: Rgb::new(0.0, 0.0, 0.0) };
    DrawTriangle { verts: [vertex(a), vertex(b), vertex(c)] }
}

fn at(x: f32, y: f32) -> Vec3 {
    Vec3::new(x, y, 0.0)
}

/// The fixture drawing, placed so none of it overlaps another and each
/// keeps clear of the page edge: a plain triangle several plane texels
/// across, a sliver a fifth of a plane texel wide — the ribbon case — and
/// the same shape wound the other way.
fn drawing() -> Vec<DrawTriangle> {
    // One plane texel in world units, on the shorter axis.
    let texel = 1.0 / PLANE_HEIGHT as f32;

    vec![
        triangle(at(-0.40, 0.30), at(-0.20, 0.30), at(-0.30, 0.10)),
        triangle(at(-0.05, 0.35), at(0.35, 0.33), at(-0.05, 0.35 - 0.2 * texel)),
        triangle(at(0.35, 0.33), at(0.35, 0.33 - 0.2 * texel), at(-0.05, 0.35 - 0.2 * texel)),
        triangle(at(-0.30, -0.10), at(-0.10, -0.30), at(-0.40, -0.30)),
    ]
}

/// The ink layer's raster: ink at alpha one where a ribbon landed and
/// transparent black where none did, which is what `fs_stroke` leaves
/// behind and all `fs_ink_plane` reads of it.
fn raster(triangles: &[DrawTriangle]) -> Vec<f32> {
    regions::ink(triangles, &camera(), RASTER_WIDTH, RASTER_HEIGHT)
}

fn raster_bytes(raster: &[f32]) -> Vec<u8> {
    raster.iter().flat_map(|&covered| [0, 0, 0, (covered * 255.0) as u8]).collect()
}

/// The oracle: the greatest alpha over each block of raster texels.
fn reduced(raster: &[f32]) -> Vec<f32> {
    let mut plane = vec![0.0f32; PLANE_WIDTH * PLANE_HEIGHT];
    for (index, covered) in plane.iter_mut().enumerate() {
        let (base_x, base_y) = ((index % PLANE_WIDTH) * FOOTPRINT, (index / PLANE_WIDTH) * FOOTPRINT);
        for y in base_y..base_y + FOOTPRINT {
            for x in base_x..base_x + FOOTPRINT {
                *covered = covered.max(raster[y * RASTER_WIDTH + x]);
            }
        }
    }

    plane
}

/// The test-only probe appended to the ink module: the plane the pass
/// wrote, carried back out at the raster's own extent so one overlay rect
/// brings it into the captured frame. Nothing samples a float plane in a
/// capture, and `textureLoad` at the block's own index reads the plane
/// texel exactly rather than interpolating between two of them.
const PROBE_WGSL: &str = r"
@group(1) @binding(0) var plane: texture_2d<f32>;
@group(1) @binding(1) var plane_sampler: sampler;

@fragment
fn fs_probe(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = vec2<i32>(position.xy) / INK_PLANE_FOOTPRINT;

    return vec4<f32>(textureLoad(plane, texel, 0).r, 0.0, 0.0, 1.0);
}
";

/// Test-only observation of the production WGSL `rail` solve. With the
/// fixture's eye five units from the origin and `shape.x` at one fifth,
/// the offset length is exactly the selected depth weight. Halving it
/// keeps both ends representable in an `Rgba8` target.
const DEPTH_PROBE_WGSL: &str = r"
@fragment
fn fs_depth_probe(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let reference = select(0.01, 100.0, position.x >= 1.0);
    let solved = rail(
        vec3<f32>(0.0, 0.0, 0.0),
        vec3<f32>(1.0, 0.0, 0.0),
        vec2<f32>(0.2, 0.0),
        reference,
    );

    return vec4<f32>(length(solved.offset) * 0.5, 0.0, 0.0, 1.0);
}
";

/// Binding indices this scenario declares: the staged raster, then the
/// readable output whose texture states the reference extent.
const RASTER: u32 = 0;
const OUTPUT: u32 = 1;

/// The coverage pass in its production shape — reducing a full-extent
/// raster into a plane at the wash body's divisor — plus the probe that
/// carries that plane into a readable texture.
fn probed_program() -> ProgramRegister {
    let mut register = ProgramRegister {
        // The skinning prelude rides along, as `stroke::program` appends
        // it: the ribbon stage poses the pen from `params.bones`, so the
        // module does not compile without it.
        wgsl: format!("{}\n{}", stroke::STROKE_WGSL, program::SKIN_WGSL),
        bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        transients: vec![stroke::ink_plane_slot()],
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes: vec![ProgramPass {
            stage: PassStage::Fragment,
            entry_point: "fs_ink_plane".to_owned(),
            inputs: vec![InputSlot::Binding { index: RASTER }],
            output: OutputSlot::Transient { index: 0 },
            uniform_offset: 0,
            uniform_length: stroke::StrokeUniforms::BYTES,
            repeat: None,
        }],
    };
    let output = append_capture_probe(
        &mut register,
        PROBE_WGSL,
        vec![InputSlot::Transient { index: 0 }],
        stroke::StrokeUniforms::BYTES,
    );
    assert_eq!(output, OUTPUT, "the probe binding layout changed");

    register
}

/// The shipped stroke module with only a test entry point added: no
/// substitute clamp or rail arithmetic enters the GPU side of the test.
fn depth_probe_program() -> ProgramRegister {
    ProgramRegister {
        wgsl: format!("{}\n{}\n{DEPTH_PROBE_WGSL}", stroke::STROKE_WGSL, program::SKIN_WGSL),
        bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        transients: Vec::new(),
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes: vec![ProgramPass {
            stage: PassStage::Fragment,
            entry_point: "fs_depth_probe".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: stroke::StrokeUniforms::BYTES,
            repeat: None,
        }],
    }
}

fn create_texture(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateTexture) -> u32 {
    let created = harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("create sequence");
    match created.reply::<CreateTextureResult>(label).expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture ({label}) failed: {error}"),
    }
}

/// One reduction of `raster`, read back as coverage per plane texel
/// through the established overlay-and-capture route.
fn develop(harness: &mut SubstrateHarness, raster: &[f32]) -> Vec<f32> {
    let rgba = |pixels, usage, sampling| CreateTexture {
        width: RASTER_WIDTH as u32,
        height: RASTER_HEIGHT as u32,
        format: TextureFormat::Rgba8,
        sampling,
        usage,
        pixels,
    };
    let staged = create_texture(
        harness,
        "create_raster",
        &rgba(raster_bytes(raster), TextureUsage::Sampled, TextureSampling::Nearest),
    );
    let output =
        create_texture(harness, "create_output", &rgba(Vec::new(), TextureUsage::Writable, TextureSampling::Linear));

    let program_id = match harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", &probed_program()))])
        .expect("register sequence")
        .reply::<ProgramRegisterResult>("register")
        .expect("decode ProgramRegisterResult")
    {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    };

    // Neither stage reads the block, but the module declares it, so the
    // dispatch carries the one a production encoder would have written.
    let uniforms = stroke::StrokeUniforms {
        view_proj: camera(),
        eye: Vec3::new(0.0, 0.0, 5.0),
        bias: 0.0,
        field: (PLANE_WIDTH as u32, PLANE_HEIGHT as u32),
        bones: deform::bone_uniform(&[]),
    }
    .encode();
    let dispatch = ProgramDispatch { program_id, bindings: vec![staged, output], geometries: Vec::new(), uniforms };

    // Drawn texel-for-pixel at the window's top-left, so each pixel centre
    // lands on a texel centre and the sampler returns it exactly.
    let overlay = DrawTexturedQuads {
        texture_id: output,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        layer: 0,
        quads: vec![TexturedQuad {
            x: 0.0,
            y: 0.0,
            width: RASTER_WIDTH as f32,
            height: RASTER_HEIGHT as f32,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    };
    let pre = vec![envelope("aether.render", &dispatch), envelope("aether.render", &overlay)];
    let captured = harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture");
    let image = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

    let mut plane = Vec::with_capacity(PLANE_WIDTH * PLANE_HEIGHT);
    for y in 0..PLANE_HEIGHT {
        for x in 0..PLANE_WIDTH {
            // The pass writes 1.0 or leaves the clear, so the byte is
            // saturated either way and the sRGB transfer round-trips it
            // exactly — no inverse needed, just the two ends apart. Any
            // pixel of the block reads the same plane texel.
            plane.push(f32::from(rgba_at(&image, (x * FOOTPRINT) as u32, (y * FOOTPRINT) as u32)[0]) / 255.0);
        }
    }

    plane
}

/// The two clamped weights the production WGSL solves, decoded back to
/// linear after the writable-target and capture round trips.
fn depth_weights(harness: &mut SubstrateHarness) -> [f32; 2] {
    let output = create_texture(
        harness,
        "create_depth_output",
        &CreateTexture {
            width: 2,
            height: 1,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        },
    );
    let program_id = match harness
        .execute(vec![(
            "register_depth_probe",
            HarnessOp::send_and_await_reply("aether.render", &depth_probe_program()),
        )])
        .expect("register depth probe sequence")
        .reply::<ProgramRegisterResult>("register_depth_probe")
        .expect("decode depth probe ProgramRegisterResult")
    {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register depth probe failed: {reason}"),
    };
    let eye = Vec3::new(0.0, 0.0, 5.0);
    let uniforms =
        stroke::StrokeUniforms { view_proj: camera(), eye, bias: 0.0, field: (1, 1), bones: deform::bone_uniform(&[]) }
            .encode();
    let dispatch = ProgramDispatch { program_id, bindings: vec![output], geometries: Vec::new(), uniforms };
    let overlay = DrawTexturedQuads {
        texture_id: output,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        layer: 0,
        quads: vec![TexturedQuad {
            x: 0.0,
            y: 0.0,
            width: 2.0,
            height: 1.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    };
    let pre = vec![envelope("aether.render", &dispatch), envelope("aether.render", &overlay)];
    let captured =
        harness.execute(vec![("snap_depth", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture depth probe");
    let image = decode_png(captured.captured("snap_depth").expect("depth probe ran")).expect("decode depth probe png");

    [0, 1].map(|x| srgb_byte_to_linear(rgba_at(&image, x, 0)[0]) / 0.5)
}

/// A plane rendered as text, so a failure shows the shape of the
/// disagreement rather than one coordinate.
fn sketch(plane: &[f32]) -> String {
    let mut out = String::new();
    for y in 0..PLANE_HEIGHT {
        for x in 0..PLANE_WIDTH {
            out.push(if plane[y * PLANE_WIDTH + x] > 0.5 {
                '#'
            } else {
                '.'
            });
        }
        out.push('\n');
    }

    out
}

fn harness() -> SubstrateHarness {
    SubstrateHarness::builder().size(RASTER_WIDTH as u32, RASTER_HEIGHT as u32).with_render().build().expect("boot")
}

#[test]
fn the_coverage_plane_is_the_raster_reduced() {
    if !require_wgpu_only() {
        return;
    }
    let raster = raster(&drawing());
    let expected = reduced(&raster);

    // Guards the fixture, not the pass: a drawing the reduction barely
    // claims would let a broken footprint pass by agreeing on emptiness.
    let claimed = expected.iter().filter(|&&at| at > 0.5).count();
    assert!(claimed > 400, "test setup: the fixture should claim a useful area, claimed {claimed}");

    let developed = develop(&mut harness(), &raster);
    let disagreeing: Vec<(usize, usize)> = developed
        .iter()
        .zip(&expected)
        .enumerate()
        .filter(|(_, (got, want))| (**got > 0.5) != (**want > 0.5))
        .map(|(index, _)| (index % PLANE_WIDTH, index / PLANE_WIDTH))
        .collect();

    assert!(
        disagreeing.is_empty(),
        "the coverage pass reduces {} texels differently from the maximum it takes ({disagreeing:?})\nGPU:\n{}\noracle:\n{}",
        disagreeing.len(),
        sketch(&developed),
        sketch(&expected),
    );
}

#[test]
fn a_stroke_thinner_than_a_plane_texel_reduces_to_a_continuous_line() {
    if !require_wgpu_only() {
        return;
    }
    // The sliver alone, away from every other case.
    let raster = raster(&drawing()[1..3]);

    let developed = develop(&mut harness(), &raster);
    let rows: Vec<usize> =
        (0..PLANE_HEIGHT).filter(|&y| (0..PLANE_WIDTH).any(|x| developed[y * PLANE_WIDTH + x] > 0.5)).collect();
    assert!(!rows.is_empty(), "the sliver reduced to nothing at all:\n{}", sketch(&developed));

    // Every column the line spans is claimed, with no gap along it.
    let claimed: Vec<usize> =
        (0..PLANE_WIDTH).filter(|&x| (0..PLANE_HEIGHT).any(|y| developed[y * PLANE_WIDTH + x] > 0.5)).collect();
    let (first, last) = (claimed[0], claimed[claimed.len() - 1]);
    assert_eq!(
        claimed.len(),
        last - first + 1,
        "the sliver reduces to a dashed line — {} of {} columns between {first} and {last} claimed (row {}):\n{}",
        claimed.len(),
        last - first + 1,
        rows[0],
        sketch(&developed),
    );
    assert!(last - first > 20, "test setup: the sliver should span a useful run, spanned {}", last - first);
}

#[test]
fn the_depth_clamp_saturates_like_the_rust_rail_on_both_sides() {
    if !require_wgpu_only() {
        return;
    }
    let eye = Vec3::new(0.0, 0.0, 5.0);
    let anchor = ribbon::Anchor { pos: Vec3::ZERO, along: Vec3::new(1.0, 0.0, 0.0), half: 0.2, drift: 0.0 };
    let references = [0.01, 100.0];
    let expected = references.map(|reference| ribbon::rail(&anchor, reference, eye).1.length());

    // Prove the fixture reaches the saturated branches rather than
    // merely comparing two points inside the clamp.
    let raw = references.map(|reference| reference / eye.length());
    assert!(expected[0] > raw[0], "test setup: the near-side ratio did not reach the floor");
    assert!(expected[1] < raw[1], "test setup: the far-side ratio did not reach the ceiling");

    let got = depth_weights(&mut harness());
    // One half-step from the Rgba8 write and one sRGB byte round trip,
    // doubled because the probe encoded each weight at half scale.
    let tolerance = 2.0 / 255.0 / 0.5;
    for (side, (got, expected)) in ["floor", "ceiling"].into_iter().zip(got.into_iter().zip(expected)) {
        assert!(
            (got - expected).abs() <= tolerance,
            "the shipped WGSL {side} disagrees with ribbon::rail: got {got}, expected {expected}, tolerance {tolerance}",
        );
    }
}
