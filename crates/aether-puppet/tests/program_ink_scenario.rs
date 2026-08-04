//! Coverage parity for the ink pass (iamacoffeepot/aether#4410,
//! ADR-0171): the one rasterizing pass in the wash program developed
//! over ribbon geometry, against `easel::regions::ink` on the identical
//! triangles and camera.
//!
//! Unlike every sibling scenario the budget here is zero. The oracle is
//! a binary test — a pixel is claimed or it is not — so there is no
//! accumulator to drift, no quantization to absorb, and the two answers
//! either agree texel for texel or the transcription is wrong. The
//! fragment stage evaluates the oracle's own three edge functions at
//! `@builtin(position)`, which is the oracle's own pixel centre, so
//! agreement is by construction rather than by tuning.
//!
//! What the cases are for: a plain triangle covers the interior rule; a
//! sub-pixel-wide sliver covers the half-pixel slack (#4356) that the
//! whole plane exists for — under a bare pixel-centre test it vanishes,
//! and a vanished stroke is what makes the flow field read a dashed
//! drawing; the reversed winding covers the oracle's sign handling; and
//! the near-parallel pair covers the miter the vertex stage widens by.

// Skip diagnostics go to stderr so `cargo test -- --nocapture` surfaces
// them next to the test name.
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
// The fixture's world coordinates read as "a fifth of a pixel down from
// the rail", which is the geometry the case is about; `mul_add` would
// hide it.
#![allow(clippy::suboptimal_flops)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, rgba_at};
use aether_harness_substrate_capture::visual::decode_png;
use aether_kinds::QuadSpace;
use aether_math::{Mat4, Rgb, Rgba, Vec2, Vec3};
use aether_puppet::easel::program::ink;
use aether_puppet::easel::regions;
use aether_render::QuadBlend;
use aether_render::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DrawTexturedQuads, DrawTriangle,
    InputSlot, OutputSlot, PassStage, ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, SlotExtent,
    SlotSpec, TextureFormat, TextureSampling, TextureUsage, TexturedQuad, Vertex,
};

/// The canvas the parity develops at. Small enough to compare every
/// texel by hand, large enough that a sliver spans a useful run of them.
const PLANE_WIDTH: usize = 96;
const PLANE_HEIGHT: usize = 64;

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
/// `[-0.5, 0.5]` lands on the page at a known pixel and the fixtures can
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

/// The fixture drawing: the four cases the module doc names, placed so
/// none overlaps another and each keeps clear of the page edge.
fn drawing() -> Vec<DrawTriangle> {
    // One page pixel in world units, on the shorter axis.
    let pixel = 1.0 / PLANE_HEIGHT as f32;

    vec![
        // A plain triangle, comfortably many pixels across.
        triangle(at(-0.40, 0.30), at(-0.20, 0.30), at(-0.30, 0.10)),
        // A sliver a fifth of a pixel wide: the ribbon case. A bare
        // pixel-centre test finds almost none of it.
        triangle(at(-0.05, 0.35), at(0.35, 0.33), at(-0.05, 0.35 - 0.2 * pixel)),
        triangle(at(0.35, 0.33), at(0.35, 0.33 - 0.2 * pixel), at(-0.05, 0.35 - 0.2 * pixel)),
        // The same shape wound the other way.
        triangle(at(-0.30, -0.10), at(-0.10, -0.30), at(-0.40, -0.30)),
        // A stubby near-degenerate wedge, where the miter reaches
        // furthest past the geometry.
        triangle(at(0.05, -0.15), at(0.34, -0.16), at(0.05, -0.15 - 0.6 * pixel)),
    ]
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

/// The test-only probe appended to the ink module: the coverage
/// transient the pass writes, read back out as an `Rgba8` texel so a
/// single overlay rect carries it into the captured frame. Nothing
/// samples a float plane in a capture, and the coverage is binary, so
/// the probe is a straight copy into the red channel.
const PROBE_WGSL: &str = r"
@group(1) @binding(0) var coverage: texture_2d<f32>;
@group(1) @binding(1) var coverage_sampler: sampler;

@fragment
fn fs_probe(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(coverage, coverage_sampler, uv).r, 0.0, 0.0, 1.0);
}
";

/// The ink pass in its production shape — drawing into a transient —
/// plus the probe that carries that transient into a readable texture.
fn probed_program() -> ProgramRegister {
    ProgramRegister {
        wgsl: format!("{}\n{PROBE_WGSL}", ink::INK_WGSL),
        bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        transients: vec![ink::plane_slot()],
        geometries: vec![ink::geometry_slot()],
        depth_transients: Vec::new(),
        passes: vec![
            ink::coverage_pass(0, OutputSlot::Transient { index: 0 }, 0),
            ProgramPass {
                stage: PassStage::Fragment,
                entry_point: "fs_probe".to_owned(),
                inputs: vec![InputSlot::Transient { index: 0 }],
                output: OutputSlot::Binding { index: 0 },
                uniform_offset: 0,
                uniform_length: 0,
                repeat: None,
            },
        ],
    }
}

/// One develop of the ink pass over `triangles`, read back as coverage
/// per texel through the established overlay-and-capture route.
fn develop(harness: &mut SubstrateHarness, triangles: &[DrawTriangle]) -> Vec<f32> {
    let output = create_texture(
        harness,
        "create_output",
        &CreateTexture {
            width: PLANE_WIDTH as u32,
            height: PLANE_HEIGHT as u32,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        },
    );

    let geometry = {
        let mail = CreateGeometry {
            layout: ink::geometry_slot().layout,
            vertices: ink::vertices(triangles),
            indices: ink::indices(triangles),
        };
        let created = harness
            .execute(vec![("create_geometry", HarnessOp::send_and_await_reply("aether.render", &mail))])
            .expect("create_geometry sequence");
        match created.reply::<CreateGeometryResult>("create_geometry").expect("decode CreateGeometryResult") {
            CreateGeometryResult::Ok { geometry_id } => geometry_id,
            CreateGeometryResult::Err { reason } => panic!("create_geometry failed: {reason}"),
        }
    };

    let register = probed_program();
    let program_id = match harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", &register))])
        .expect("register sequence")
        .reply::<ProgramRegisterResult>("register")
        .expect("decode ProgramRegisterResult")
    {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    };

    let half_size = Vec2::new(PLANE_WIDTH as f32 * 0.5, PLANE_HEIGHT as f32 * 0.5);
    let uniforms = ink::InkUniforms { view_proj: camera(), half_size }.encode().to_vec();
    let dispatch = ProgramDispatch { program_id, bindings: vec![output], geometries: vec![geometry], uniforms };

    // Drawn texel-for-pixel at the window's top-left, so each pixel
    // centre lands on a texel centre and the sampler returns it exactly.
    let overlay = DrawTexturedQuads {
        texture_id: output,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![TexturedQuad {
            x: 0.0,
            y: 0.0,
            width: PLANE_WIDTH as f32,
            height: PLANE_HEIGHT as f32,
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
            // exactly — no inverse needed, just the two ends apart.
            plane.push(f32::from(rgba_at(&image, x as u32, y as u32)[0]) / 255.0);
        }
    }
    plane
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

#[test]
fn the_ink_pass_claims_the_pixels_the_oracle_claims() {
    if !require_wgpu_only() {
        return;
    }
    let triangles = drawing();
    let expected = regions::ink(&triangles, &camera(), PLANE_WIDTH, PLANE_HEIGHT);

    // Guards the fixture, not the pass: a drawing the oracle barely
    // claims would let a broken transcription pass by agreeing on
    // emptiness, and the sliver is the case that matters most.
    let claimed = expected.iter().filter(|&&at| at > 0.5).count();
    assert!(claimed > 400, "test setup: the fixture should claim a useful area, claimed {claimed}");

    let mut harness = SubstrateHarness::builder().size(128, 96).with_render().build().expect("boot");
    let developed = develop(&mut harness, &triangles);

    let disagreeing: Vec<(usize, usize)> = developed
        .iter()
        .zip(&expected)
        .enumerate()
        .filter(|(_, (got, want))| (**got > 0.5) != (**want > 0.5))
        .map(|(index, _)| (index % PLANE_WIDTH, index / PLANE_WIDTH))
        .collect();

    // Where the two may differ, and why that is the whole allowance: an
    // edge function evaluated at a pixel centre that sits within a float
    // epsilon of the edge itself is a tie, and the CPU and the GPU round
    // it independently — the same expression, different orders and a
    // different `hypot`. A tie can only happen on the oracle's own
    // boundary, so every disagreement must land there. One that does not
    // is a real divergence: a wrong slack, a wrong winding, or a wrong
    // sample point moves interior texels, which no rounding can.
    let interior: Vec<(usize, usize)> =
        disagreeing.iter().copied().filter(|&at| !on_oracle_boundary(&expected, at)).collect();
    assert!(
        interior.is_empty(),
        "the ink pass disagrees with the oracle away from any edge, at {} texels ({interior:?})\nGPU:\n{}\noracle:\n{}",
        interior.len(),
        sketch(&developed),
        sketch(&expected),
    );

    // And ties stay rare: a transcription that agrees only in the large
    // would still satisfy the boundary rule if the boundary were most of
    // the drawing.
    let boundary = (0..PLANE_WIDTH * PLANE_HEIGHT)
        .filter(|&index| on_oracle_boundary(&expected, (index % PLANE_WIDTH, index / PLANE_WIDTH)))
        .count();
    assert!(
        disagreeing.len() * 20 <= boundary,
        "the ink pass ties differently on {} of {boundary} boundary texels, more than rounding explains\nGPU:\n{}\noracle:\n{}",
        disagreeing.len(),
        sketch(&developed),
        sketch(&expected),
    );
}

/// Whether `(x, y)` touches both a claimed and an unclaimed texel in the
/// oracle's plane — the only place an edge-function tie can fall.
fn on_oracle_boundary(oracle: &[f32], (x, y): (usize, usize)) -> bool {
    let mut claimed = false;
    let mut clear = false;
    for ny in y.saturating_sub(1)..=(y + 1).min(PLANE_HEIGHT - 1) {
        for nx in x.saturating_sub(1)..=(x + 1).min(PLANE_WIDTH - 1) {
            if oracle[ny * PLANE_WIDTH + nx] > 0.5 {
                claimed = true;
            } else {
                clear = true;
            }
        }
    }
    claimed && clear
}

/// Tripwire: the slack is what the plane exists for (#4356). If a future
/// change makes the pass a bare pixel-centre rasterize, the sliver stops
/// being a continuous line and the flow field reads a dashed drawing —
/// which the parity test above would also catch, but only while the
/// oracle keeps its own slack. This states the property directly.
#[test]
fn a_sub_pixel_sliver_bakes_as_a_continuous_line() {
    if !require_wgpu_only() {
        return;
    }
    // The sliver alone, away from every other case.
    let sliver = drawing()[1..3].to_vec();

    let mut harness = SubstrateHarness::builder().size(128, 96).with_render().build().expect("boot");
    let developed = develop(&mut harness, &sliver);

    let rows: Vec<usize> =
        (0..PLANE_HEIGHT).filter(|&y| (0..PLANE_WIDTH).any(|x| developed[y * PLANE_WIDTH + x] > 0.5)).collect();
    assert!(!rows.is_empty(), "the sliver baked nothing at all:\n{}", sketch(&developed));

    // Every column the line spans is claimed, with no gap along it.
    let row = rows[0];
    let claimed: Vec<usize> =
        (0..PLANE_WIDTH).filter(|&x| (0..PLANE_HEIGHT).any(|y| developed[y * PLANE_WIDTH + x] > 0.5)).collect();
    let (first, last) = (claimed[0], claimed[claimed.len() - 1]);
    assert_eq!(
        claimed.len(),
        last - first + 1,
        "the sliver bakes as a dashed line — {} of {} columns between {first} and {last} claimed (row {row}):\n{}",
        claimed.len(),
        last - first + 1,
        sketch(&developed),
    );
    assert!(last - first > 20, "test setup: the sliver should span a useful run, spanned {}", last - first);
}
