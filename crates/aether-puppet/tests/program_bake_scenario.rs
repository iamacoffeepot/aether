//! GPU-versus-CPU parity for the plane bake (iamacoffeepot/aether#4411,
//! #4412, ADR-0171): `easel/program/bake.rs`'s channel-packed draw pass
//! driven through the `aether.render` mail surface against
//! `easel::regions::rasterize` on the identical mesh, scores, settings
//! and camera.
//!
//! # What is being compared, and in what space
//!
//! The bake writes one `Rgba8` plane — R class, G tone, B facing — and
//! nothing reads pixels back to the CPU, the only pixel exit in the
//! engine being the frame capture. So the plane is observed the way
//! every parity scenario here observes one: a test-only probe pass
//! copies it into a second `Rgba8` binding, the overlay draws that
//! texel-for-pixel, and the capture's bytes are decoded back through the
//! inverse sRGB transfer.
//!
//! The probe used to do the packing — three `R32Float` planes into one
//! texel — and now does nothing but force alpha to one ([`PROBE_WGSL`]
//! says why the capture needs that and the plane's consumers do not). So
//! what is compared is the shipped texel's own three channels rather
//! than a re-encoding of them.
//!
//! The instrument's own floor, stated: `class` rides as `class / 255`,
//! which an `Rgba8Unorm` store carries exactly and the sRGB round trip
//! returns to the same integer with margin to spare, so the class plane
//! is compared *exactly*. `tone` and `facing` quantize once at the store
//! and once at the encode, about one part in 255 of their range each —
//! the same floor the probe carried, since the probe quantized into the
//! same eight bits.
//!
//! # Tone is compared against a clamped oracle
//!
//! `Settings::tone` is unclamped by contract: the face lift carries it
//! past one, and the CPU plane holds whatever it computes. An 8-bit
//! unorm channel cannot, so the packed plane clips at one and the
//! comparison below clips the oracle the same way. That is a restatement
//! of the plane's declared range, not a widened tolerance — every
//! consumer runs tone through `smoothstep(lit, SHADOWED, tone)` whose
//! largest `lit` across the palette is 0.92, so a tone at or above one
//! already saturates and nothing downstream can tell 1.0 from 1.6.
//! [`the_packed_tone_channel_clips_below_every_consumer`] pins that
//! claim so a palette edit past one cannot quietly make the clip
//! observable.
//!
//! # The tolerance that matters
//!
//! Not per-pixel class equality. The GPU and the oracle resolve a pixel
//! whose centre falls within a hair of a triangle edge by different
//! rules — wgpu's top-left fill rule over its own rasterizer precision
//! against the oracle's inclusive barycentric test in `f32` — so on a
//! subject fine enough that faces are near pixel-sized, a scatter of
//! pixels along the silhouettes is expected to disagree and no amount
//! of correctness removes it. (On the shipped subject it is thirteen
//! pixels in 1.08 million.) What the wash actually consumes is the
//! boundary *after* the water blur, cut at level 0.5
//! (`field::Sheet::threshold`), so the question that decides whether the
//! two bakes paint the same picture is how far that level set moves.
//! [`level_set_drift`] measures exactly that, in pixels, as
//! `|blurred_gpu - blurred_cpu| / |grad blurred_cpu|` along the blurred
//! CPU field's own gradient — the distance you would travel to get from
//! one field's 0.5 to the other's.
//!
//! # The ignored real-subject run
//!
//! [`crossfeed_the_gpu_bake_against_the_cpu_oracle`] is the instrument
//! the shipped subject is A/B'd through, at the framing the easel
//! actually develops at. Drive it with the subject and its material
//! field placed in one directory:
//!
//! ```text
//! AETHER_CROSSFEED_DIR=/path/to/dir \
//!     cargo test -p aether-puppet --release --test program_bake_scenario \
//!     -- --ignored --nocapture
//! ```
//!
//! where the directory holds `subject.obj` and `labels.npy`. It reports
//! the measured drift and the bake's added per-frame GPU cost rather
//! than asserting a budget tuned on a synthetic figure.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

// Integration-test skip diagnostic and instrument reporting: emit via
// stderr so `cargo test -- --nocapture` surfaces them alongside the test
// name (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle and the instrument's
// own AETHER_CROSSFEED_DIR — test-harness knobs, not cap config.
#![allow(clippy::disallowed_methods)]
// A test binary is its own compilation unit, so the crate-level allows
// do not reach it. Plane indexing casts between texel indices and `f32`
// coordinates the same bounded way the easel does, and the oracle-side
// transcriptions must stay textually identical to the formulas they
// mirror, so no `mul_add` rewrites.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]

use core::fmt::Write as _;
use std::env;
use std::fs;
use std::path::Path;
use std::time::Instant;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, rgba_at};
use aether_harness_substrate_capture::visual::decode_png;
use aether_kinds::QuadSpace;
use aether_math::{Mat4, Rgba, Vec3};
use aether_puppet::easel::image;
use aether_puppet::easel::palette;
use aether_puppet::easel::program::bake::{self, BakeUniforms};
use aether_puppet::easel::regions::{self, RegionPlanes};
use aether_puppet::extract::Settings;
use aether_puppet::labels::{self, Labels};
use aether_puppet::mesh::Mesh;
use aether_render::QuadBlend;
use aether_render::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DrawTexturedQuads, InputSlot, OutputSlot,
    PassStage, ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, TextureFormat, TextureSampling,
    TextureUsage, TexturedQuad, UpdateGeometry,
};

/// The canvas the CI parity bakes at: small enough that the oracle walks
/// it in a moment, large enough that a blurred boundary has room to sit
/// a measurable distance off.
const CANVAS_WIDTH: usize = 120;
const CANVAS_HEIGHT: usize = 160;

/// The framing the easel itself develops at (`Puppet::init`'s `Look`),
/// and the field of view it projects through.
const AZIMUTH: f32 = 0.0;
const ELEVATION: f32 = 3.0;
const DISTANCE: f32 = 5.4;
const FIELD_OF_VIEW: f32 = 0.454;

/// Padding the material field was baked with, as `Puppet` reconstructs
/// the lattice (`LABEL_PAD`).
const LABEL_PAD: f32 = 0.12;

/// Ceiling the packed plane's tone channel clips at, and so the ceiling
/// the oracle is compared through. See the module header.
const TONE_CEILING: f32 = 1.0;

/// How far the water softens a region before its edge is cut
/// (`field::held_params`'s tight `water`), in pixels. The drift is
/// measured at the wash's own scale because that is the blur whose 0.5
/// level the shipped engine actually thresholds — and as an absolute
/// radius rather than through `image::tuned`, because the tuning height
/// *is* the shipped canvas height, so scaling it down to a small
/// fixture would collapse the blur to a sub-pixel no-op and quietly
/// measure nothing.
const WATER_PIXELS: f32 = 3.2;

/// Only pixels whose blurred CPU field sits within this of level 0.5
/// carry a boundary; elsewhere the field is flat and the ratio below is
/// noise over noise.
const LEVEL_BAND: f32 = 0.25;

/// What fraction of a fully-formed edge's own slope a pixel must carry
/// before its displacement is believed, against [`edge_slope`].
///
/// The displacement below is a first-order solve — a difference divided
/// by a gradient — and first-order solves come apart as the denominator
/// goes to zero. On a plateau where the blurred field is merely near
/// 0.5 without a boundary running through it, one flipped texel moves
/// the numerator by about a kernel's worth and the quotient reports a
/// boundary that has travelled pixels when nothing has moved at all.
/// The cut is a quarter of the slope a clean step produces at this
/// blur: shallower than that and the field is carrying an edge's tail,
/// not its crossing. The raw per-pixel disagreement is reported
/// unfiltered alongside, so this filter can never hide a difference —
/// only decline to express one in pixels.
const GRADIENT_FRACTION: f32 = 0.25;

/// Ceiling on how far the blur-then-threshold boundary may move, in
/// pixels. One pixel, because a differing pixel-centre fill rule can
/// disagree about exactly the pixels the boundary passes through and no
/// more — a level set that moves further is a real classification
/// difference, not a rasterization tie-break. The shipped subject
/// measures 0.585 px at its worst class and 0.003 px in the mean, and
/// shifting the argmax by one class trips this at 7.6 px, so the budget
/// sits with room on both sides of it.
const DRIFT_BUDGET: f32 = 1.0;

/// Ceiling on the share of the page that may carry a different class at
/// all, as a fraction. One percent: the honest disagreement is a scatter
/// along the silhouettes — zero on the synthetic fixture, 0.0012% on the
/// shipped subject — so this sits three orders of magnitude clear of the
/// fringe while still catching anything plane-wide.
const FRINGE_BUDGET: f32 = 0.01;

/// Ceiling on the mean absolute difference of the `tone` and `facing`
/// channels over the drawn figure, in their own units. Both are pure
/// interpolations of a per-vertex scalar, so the only honest sources of
/// difference are the two quantizations each takes — one at the packed
/// store, one at the capture's encode, about `1 / 255` apiece — and the
/// edge fringe.
const SURFACE_BUDGET: f32 = 0.02;

fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// The test-only opacity probe.
///
/// A draw pass shades the pixels its geometry covers and leaves the rest
/// at `PassLoad::Clear`, which is transparent black — so off the subject
/// the packed plane carries alpha 0, which is exactly right for its
/// consumers (they `textureLoad` a class-0, tone-0, facing-0 texel) and
/// exactly wrong for this instrument (the overlay alpha-blends, so those
/// texels would come back as whatever the framebuffer already held
/// instead of as zero). This pass carries the three channels through
/// untouched and forces alpha to one, so the capture reads the plane's
/// own bytes everywhere rather than only where the subject stands.
///
/// One `textureLoad` and no arithmetic on the quantities: what is
/// compared below is the shipped texel, not a re-encoding of it. When
/// the bake filled three `R32Float` planes the probe here did the
/// packing as well, so a cost measured with this pass in place stays
/// comparable to the one measured with that.
const PROBE_WGSL: &str = r"
@group(1) @binding(0) var baked_packed: texture_2d<f32>;

@fragment
fn fs_probe(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureLoad(baked_packed, vec2<i32>(position.xy), 0).rgb, 1.0);
}
";

/// Which binding the probe writes: the packed plane the bake fills, then
/// the opaque copy the overlay draws.
const PROBE: u32 = bake::PACKED + 1;

/// The bake's own graph plus the opacity probe: one more `Rgba8` binding
/// and one fragment pass reading the plane the draw pass just filled.
fn probed_program() -> ProgramRegister {
    let mut register = bake::program();
    register.wgsl = format!("{}\n{PROBE_WGSL}", register.wgsl);
    register.bindings.push(bake::packed_slot());
    register.passes.push(ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_probe".to_owned(),
        inputs: vec![InputSlot::Binding { index: bake::PACKED }],
        output: OutputSlot::Binding { index: PROBE },
        uniform_offset: 0,
        uniform_length: 0,
        repeat: None,
    });

    register
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

/// The two writable textures one bake dispatches into: the packed plane
/// and the probe's opaque copy of it.
///
/// Both nearest, as [`bake::packed_slot`] requires — the class channel is
/// an integer in disguise and a filter across a material boundary would
/// average the labels either side into a third. The overlay draws
/// texel-for-pixel, so the sampler returns each texel exactly and the
/// choice costs the readback nothing either way.
fn create_targets(harness: &mut SubstrateHarness, width: usize, height: usize) -> Vec<u32> {
    ["create_packed", "create_probe"]
        .into_iter()
        .map(|label| {
            create_texture(
                harness,
                label,
                &CreateTexture {
                    width: width as u32,
                    height: height as u32,
                    format: TextureFormat::Rgba8,
                    sampling: TextureSampling::Nearest,
                    usage: TextureUsage::Writable,
                    pixels: Vec::new(),
                },
            )
        })
        .collect()
}

fn create_geometry(harness: &mut SubstrateHarness, mesh: &Mesh, scores: &[[f32; labels::CLASSES]]) -> u32 {
    let mail = CreateGeometry {
        layout: bake::geometry_slot().layout,
        vertices: bake::vertices(mesh, scores, &settings()),
        indices: bake::indices(mesh),
    };
    let created = harness
        .execute(vec![("create_geometry", HarnessOp::send_and_await_reply("aether.render", &mail))])
        .expect("create_geometry sequence");
    match created.reply::<CreateGeometryResult>("create_geometry").expect("decode CreateGeometryResult") {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { reason } => panic!("create_geometry failed: {reason}"),
    }
}

fn register(harness: &mut SubstrateHarness, mail: &ProgramRegister) -> u32 {
    let registered = harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("register sequence");
    match registered.reply::<ProgramRegisterResult>("register").expect("decode ProgramRegisterResult") {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    }
}

/// The probe texture drawn texel-for-pixel at the window's top-left, so
/// pixel centres land on texel centres and the sampler returns each
/// texel exactly.
fn overlay(texture_id: u32, width: usize, height: usize) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![TexturedQuad {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

/// Invert the offscreen target's sRGB transfer: the capture's bytes are
/// the encoded framebuffer values, and the comparison space is linear.
fn srgb_byte_to_linear(byte: u8) -> f32 {
    let encoded = f32::from(byte) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Everything the bake needs of a camera at one azimuth, as the puppet
/// builds it: the eye on an orbit about the origin, and the matrix the
/// drawing would be made through.
fn camera(azimuth: f32, width: usize, height: usize) -> (Vec3, Mat4) {
    let (azimuth, elevation) = (azimuth.to_radians(), ELEVATION.to_radians());
    let (sin_a, cos_a) = azimuth.sin_cos();
    let (sin_e, cos_e) = elevation.sin_cos();
    let eye = Vec3::new(sin_a * cos_e, sin_e, cos_a * cos_e) * DISTANCE;

    let view = Mat4::look_at_rh(eye, Vec3::splat(0.0), Vec3::new(0.0, 1.0, 0.0));
    let projection = Mat4::perspective_rh(FIELD_OF_VIEW, width as f32 / height as f32, 0.05, 40.0);

    (eye, projection * view)
}

/// The lighting every bake here runs under. Fixed rather than
/// defaulted so the oracle and the GPU are known to be reading the same
/// key light.
fn settings() -> Settings {
    Settings { light: Vec3::new(0.3, 0.6, 1.0), ambient: 0.25, ..Settings::default() }
}

/// The three channels as the GPU baked them, decoded out of one capture.
struct Baked {
    class: Vec<u8>,
    tone: Vec<f32>,
    facing: Vec<f32>,
}

/// Everything one bake dispatches against, mounted once and held: the
/// registered program, the writable target, and the uploaded subject.
/// Kept together because a dispatch names all of them and nothing here
/// ever holds one without the others.
struct Rig {
    program_id: u32,
    bindings: Vec<u32>,
    geometry: u32,
    width: usize,
    height: usize,
}

impl Rig {
    fn mount(
        harness: &mut SubstrateHarness,
        mesh: &Mesh,
        scores: &[[f32; labels::CLASSES]],
        width: usize,
        height: usize,
    ) -> Self {
        Self {
            program_id: register(harness, &probed_program()),
            bindings: create_targets(harness, width, height),
            geometry: create_geometry(harness, mesh, scores),
            width,
            height,
        }
    }

    /// One dispatch of the bake, plus the overlay and capture that carry
    /// its plane back. The subject is already uploaded; the camera rides
    /// the uniform blob alone, which is the point — a turn costs eighty
    /// bytes and no re-upload at all.
    fn read_planes(&self, harness: &mut SubstrateHarness, eye: Vec3, view_proj: Mat4) -> Baked {
        let dispatch = ProgramDispatch {
            program_id: self.program_id,
            bindings: self.bindings.clone(),
            geometries: vec![self.geometry],
            uniforms: BakeUniforms { view_proj, eye }.encode().to_vec(),
        };
        let probe = self.bindings[PROBE as usize];
        let pre = vec![
            envelope("aether.render", &dispatch),
            envelope("aether.render", &overlay(probe, self.width, self.height)),
        ];

        let captured = harness
            .execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))])
            .expect("capture the baked planes");
        let image = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

        let texels = self.width * self.height;
        let mut baked = Baked {
            class: Vec::with_capacity(texels),
            tone: Vec::with_capacity(texels),
            facing: Vec::with_capacity(texels),
        };
        for y in 0..self.height {
            for x in 0..self.width {
                let texel = rgba_at(&image, x as u32, y as u32);
                baked.class.push((srgb_byte_to_linear(texel[0]) * 255.0).round() as u8);
                baked.tone.push(srgb_byte_to_linear(texel[1]));
                baked.facing.push(srgb_byte_to_linear(texel[2]));
            }
        }

        baked
    }
}

/// The steepest slope a fully-formed edge carries once softened at
/// [`WATER_PIXELS`] — measured, not assumed, by blurring a half-plane
/// step and reading its own gradient back, so the scale the drift is
/// judged against follows the blur rather than a constant that would
/// quietly stop matching it.
fn edge_slope() -> f32 {
    const SIDE: usize = 64;

    let step: Vec<f32> = (0..SIDE * SIDE).map(|i| f32::from(i % SIDE >= SIDE / 2)).collect();
    let softened = image::blur(&step, SIDE, SIDE, WATER_PIXELS);

    // Read along the middle row only: the blur's own border handling
    // makes the edges of the fixture say something about the fixture.
    let row = SIDE / 2 * SIDE;
    (1..SIDE - 1).map(|x| ((softened[row + x + 1] - softened[row + x - 1]) * 0.5).abs()).fold(0.0, f32::max)
}

/// How far one class's blur-then-threshold boundary moved between two
/// class planes, in pixels: the worst displacement and the mean over
/// every pixel that carries a boundary at all.
///
/// The displacement at a pixel is `|blurred_gpu - blurred_cpu|` divided
/// by the blurred CPU field's own gradient magnitude — a first-order
/// solve for how far along that gradient you would step to get from one
/// field's level to the other's, which is exactly "where 0.5 lands"
/// expressed in the units the question is asked in. `None` when the
/// class has no boundary in either plane.
fn level_set_drift(cpu: &[u8], gpu: &[u8], class: u8, width: usize, height: usize) -> Option<(f32, f32)> {
    let soften = |classes: &[u8]| image::blur(&palette::mask_of(classes, class), width, height, WATER_PIXELS);
    let (oracle, measured) = (soften(cpu), soften(gpu));
    let floor = edge_slope() * GRADIENT_FRACTION;

    let (mut worst, mut total, mut counted) = (0.0f32, 0.0f32, 0usize);
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let at = y * width + x;
            if (oracle[at] - 0.5).abs() > LEVEL_BAND {
                continue;
            }

            let slope_x = (oracle[at + 1] - oracle[at - 1]) * 0.5;
            let slope_y = (oracle[at + width] - oracle[at - width]) * 0.5;
            let slope = slope_x.hypot(slope_y);
            if slope < floor {
                continue;
            }

            let displacement = (measured[at] - oracle[at]).abs() / slope;
            worst = worst.max(displacement);
            total += displacement;
            counted += 1;
        }
    }

    (counted > 0).then(|| (worst, total / counted as f32))
}

/// Mean absolute difference of two planes over the pixels the oracle
/// actually drew on, and the worst single pixel there. Restricted to
/// drawn pixels because the bare paper is zero in both and averaging it
/// in would hide a real difference under a field of exact agreement.
fn surface_drift(cpu: &[f32], gpu: &[f32], drawn: &[u8]) -> (f32, f32) {
    let (mut worst, mut total, mut counted) = (0.0f32, 0.0f32, 0usize);
    for ((&expected, &actual), &class) in cpu.iter().zip(gpu).zip(drawn) {
        if class == 0 {
            continue;
        }
        let difference = (expected - actual).abs();
        worst = worst.max(difference);
        total += difference;
        counted += 1;
    }

    (
        worst,
        if counted > 0 {
            total / counted as f32
        } else {
            0.0
        },
    )
}

/// Every class the oracle put on the page, so a drift check runs over
/// the materials that are actually there rather than the whole
/// vocabulary.
fn drawn_classes(planes: &RegionPlanes) -> Vec<u8> {
    let mut present: Vec<u8> = planes.class.iter().copied().filter(|&class| class != 0).collect();
    present.sort_unstable();
    present.dedup();
    present
}

/// Hold one GPU bake against the oracle on identical inputs, reporting
/// what it measured. Returns the worst level-set drift seen over every
/// class present.
fn assert_parity(context: &str, oracle: &RegionPlanes, baked: &Baked, width: usize, height: usize) -> f32 {
    let classes = drawn_classes(oracle);
    assert!(!classes.is_empty(), "{context}: the oracle drew nothing, so there is no parity to measure");

    // A pixel whose centre sits on a triangle edge is resolved by two
    // different tie-break rules and is expected to differ, so this is
    // not asserted per-pixel — but it is asserted in bulk. The fringe
    // is a scatter along the silhouettes, bounded by their perimeter,
    // and it measures zero on this fixture and 0.0012% of the page on
    // the shipped subject; a whole-page disagreement is a different
    // animal entirely. The ceiling sits far above any fringe and far
    // below any systematic difference, because the level-set check
    // below cannot see one: it walks only the classes the oracle drew,
    // so a plane that came back wrong *everything else* — a mis-decoded
    // background, a readback reading the wrong texture — leaves every
    // drift at zero while the picture is garbage. That is the bug this
    // line catches, and it caught it.
    let differing = oracle.class.iter().zip(&baked.class).filter(|(expected, actual)| expected != actual).count();
    let drawn = oracle.class.iter().filter(|&&class| class != 0).count();
    let share = differing as f32 / oracle.class.len() as f32;
    eprintln!(
        "{context}: {differing} of {} pixels carry a different class ({:.4}% of the page, {:.4}% of the figure)",
        oracle.class.len(),
        100.0 * share,
        100.0 * differing as f32 / drawn.max(1) as f32,
    );
    assert!(
        share <= FRINGE_BUDGET,
        "{context}: {:.2}% of the page carries a different class, past the {:.2}% an edge fringe can accou\
         nt for — that is a plane-wide difference, not a rasterization tie-break",
        100.0 * share,
        100.0 * FRINGE_BUDGET,
    );

    let mut worst_drift = 0.0f32;
    let mut measured = 0usize;
    for class in classes {
        let Some((worst, mean)) = level_set_drift(&oracle.class, &baked.class, class, width, height) else {
            continue;
        };
        measured += 1;
        eprintln!("{context}: class {class} level-0.5 drift — worst {worst:.3} px, mean {mean:.3} px");
        assert!(
            worst <= DRIFT_BUDGET,
            "{context}: class {class}'s blur-then-threshold boundary moved {worst:.3} px, past the {DRIFT_BUDGET} px \
             budget — that is a classification difference, not an edge tie-break",
        );
        worst_drift = worst_drift.max(worst);
    }
    // Without this the whole class check is skippable in silence: a
    // blur radius that rounds to nothing, or a fixture whose regions
    // are smaller than the softening, leaves no pixel near level 0.5
    // and every `continue` above fires.
    assert!(measured > 0, "{context}: no class carried a blur-then-threshold boundary, so nothing was measured");

    // Tone through the packed channel's own ceiling: the plane clips
    // there and so must the oracle it is held against, or the comparison
    // charges the bake for a range it never claimed to carry. `facing`
    // is in `[0, 1]` already and passes through untouched.
    let clipped: Vec<f32> = oracle.tone.iter().map(|&at| at.min(TONE_CEILING)).collect();
    for (plane, cpu, gpu) in [("tone", &clipped, &baked.tone), ("facing", &oracle.facing, &baked.facing)] {
        let (worst, mean) = surface_drift(cpu, gpu, &oracle.class);
        eprintln!("{context}: {plane} drift — worst {worst:.4}, mean {mean:.4}");
        assert!(
            mean <= SURFACE_BUDGET,
            "{context}: the {plane} plane's mean drift {mean:.4} is past the {SURFACE_BUDGET} budget — both sides \
             interpolate one per-vertex scalar, so nothing but the readback's quantization belongs here",
        );
    }

    worst_drift
}

/// Turn a point off every axis, so no edge of the fixture below lands
/// parallel to a pixel row or column.
fn turned(p: Vec3) -> Vec3 {
    let (sin_z, cos_z) = 0.27f32.sin_cos();
    let (sin_y, cos_y) = 0.42f32.sin_cos();
    let spun = Vec3::new(p.x * cos_z - p.y * sin_z, p.x * sin_z + p.y * cos_z, p.z);

    Vec3::new(spun.x * cos_y + spun.z * sin_y, spun.y, spun.z * cos_y - spun.x * sin_y)
}

/// The synthetic subject: a turned octahedron standing in front of a
/// slanted backdrop.
///
/// Every property here is load-bearing for what the parity claims.
/// Turned, because axis-aligned edges let wgpu's top-left fill rule and
/// the oracle's barycentric test agree by construction — the fixture has
/// to be able to disagree before agreement means anything. Faceted,
/// because a flat subject gives every pixel the same normal and the
/// tone plane reduces to a constant nothing can be wrong about. Two
/// bodies at different depths, so the shared depth slot is actually
/// resolving an occlusion. And with the split field below, the material
/// boundary runs diagonally *across* faces rather than between them,
/// which is the case that broke the nearest-voxel classification
/// (issue 4399).
fn synthetic_subject() -> Mesh {
    let reach = 0.75;
    let corners = [
        Vec3::new(reach, 0.0, 0.0),
        Vec3::new(-reach, 0.0, 0.0),
        Vec3::new(0.0, reach, 0.0),
        Vec3::new(0.0, -reach, 0.0),
        Vec3::new(0.0, 0.0, reach),
        Vec3::new(0.0, 0.0, -reach),
    ];
    let backdrop = [
        Vec3::new(-1.1, -1.1, -0.7),
        Vec3::new(1.1, -1.1, -1.0),
        Vec3::new(1.1, 1.1, -1.1),
        Vec3::new(-1.1, 1.1, -0.8),
    ];

    let mut text = String::new();
    for point in corners.into_iter().chain(backdrop) {
        let at = turned(point);
        writeln!(text, "v {} {} {}", at.x, at.y, at.z).expect("format vertex");
    }
    for [a, b, c] in
        [[0, 2, 4], [2, 1, 4], [1, 3, 4], [3, 0, 4], [2, 0, 5], [1, 2, 5], [3, 1, 5], [0, 3, 5], [6, 7, 8], [6, 8, 9]]
    {
        writeln!(text, "f {} {} {}", a + 1, b + 1, c + 1).expect("format face");
    }

    Mesh::from_obj_bytes(text.as_bytes(), 0).expect("synthetic subject")
}

/// A material field over the subject's own bounds: a `2x2x2` lattice
/// whose x half decides the class, so a face straddling `x = 0` carries
/// a boundary the indicators have to place per pixel.
fn split_field(mesh: &Mesh) -> Labels {
    // Ten bytes of `.npy` preamble, of which only the little-endian
    // header length at [8..10] is read; zero puts the cells first.
    let mut bytes = vec![0u8; 10];
    bytes.extend([labels::HAIR; 4]);
    bytes.extend([labels::SKIN; 4]);

    Labels::parse(&bytes, mesh.min, mesh.max, LABEL_PAD).expect("split field")
}

/// Tripwire: the packed tone channel's ceiling stays above every
/// consumer's saturation point.
///
/// Packing tone into an 8-bit unorm clips it at one, and the parity
/// check above clips the oracle to match. That is honest only while no
/// consumer can see past the clip — every one of them runs tone through
/// `smoothstep(lit, SHADOWED, tone)`, which saturates at `lit`. Raise a
/// material's `shade_lit` to or past one and the clipped range becomes
/// observable: the wash would shade that material off a tone the plane
/// can no longer distinguish, while the parity scenario keeps passing
/// because it clips the oracle too. Nothing else would notice.
#[test]
fn the_packed_tone_channel_clips_below_every_consumer() {
    for material in palette::MATERIALS {
        let lit = material.shade_lit.unwrap_or(palette::LIT);
        assert!(
            lit < TONE_CEILING,
            "material class {} is fully lit at {lit}, at or past the {TONE_CEILING} the packed tone channel clips \
             at — the bake can no longer carry the tone this material shades from",
            material.class,
        );
    }
}

/// Tripwire: the GPU bake and the CPU oracle place the same boundaries.
///
/// The bug this pins is the whole point of the rung — a bake that
/// rasterizes plausibly but classifies, lights or depth-resolves
/// differently from the oracle would look perfectly reasonable in
/// isolation and repaint the subject wrong the day the wash switches
/// onto it. Every quantity is compared where it is consumed: the class
/// plane through the water blur's 0.5 level, tone and facing as
/// themselves.
#[test]
fn the_gpu_bake_places_the_same_boundaries_as_the_cpu_oracle() {
    if !require_wgpu_only() {
        return;
    }

    let mesh = synthetic_subject();
    let scores = split_field(&mesh).vertex_scores(&mesh.positions);
    let (eye, view_proj) = camera(AZIMUTH, CANVAS_WIDTH, CANVAS_HEIGHT);
    let oracle = regions::rasterize(&mesh, &scores, &settings(), eye, &view_proj, CANVAS_WIDTH, CANVAS_HEIGHT);

    let mut harness = SubstrateHarness::builder()
        .size(CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32)
        .with_render()
        .build()
        .expect("harness");
    let rig = Rig::mount(&mut harness, &mesh, &scores, CANVAS_WIDTH, CANVAS_HEIGHT);

    let baked = rig.read_planes(&mut harness, eye, view_proj);

    assert_parity("synthetic", &oracle, &baked, CANVAS_WIDTH, CANVAS_HEIGHT);
}

/// Tripwire: the bake follows vertices that move, through the re-upload
/// path and nothing else.
///
/// The rung's standing requirement is that nothing is keyed on the
/// subject being the one from last frame (a pose is coming). The
/// failure this catches is a bake that quietly holds the geometry it
/// first realized — a stale GPU buffer behind a live `geometry_id` — so
/// the planes keep describing a subject that has already moved while
/// every other assertion here still passes. The camera is held fixed
/// and only the vertices change, so the oracle's own answer moving is
/// the whole signal.
#[test]
fn a_re_uploaded_subject_re_bakes_from_its_new_vertices() {
    if !require_wgpu_only() {
        return;
    }

    let mesh = synthetic_subject();
    let scores = split_field(&mesh).vertex_scores(&mesh.positions);
    let (eye, view_proj) = camera(AZIMUTH, CANVAS_WIDTH, CANVAS_HEIGHT);

    let mut harness = SubstrateHarness::builder()
        .size(CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32)
        .with_render()
        .build()
        .expect("harness");
    let rig = Rig::mount(&mut harness, &mesh, &scores, CANVAS_WIDTH, CANVAS_HEIGHT);
    let before = rig.read_planes(&mut harness, eye, view_proj);

    // The pose: every vertex swung a third of the frame to the left,
    // scores and all, exactly as a deforming subject would arrive.
    let mut posed = synthetic_subject();
    for position in &mut posed.positions {
        position.x -= 0.45;
    }
    harness
        .execute(vec![(
            "update_geometry",
            HarnessOp::send_and_settle(
                "aether.render",
                &UpdateGeometry {
                    geometry_id: rig.geometry,
                    vertices: bake::vertices(&posed, &scores, &settings()),
                    indices: bake::indices(&posed),
                },
            ),
        )])
        .expect("update_geometry sequence");

    let after = rig.read_planes(&mut harness, eye, view_proj);
    assert_ne!(before.class, after.class, "a re-uploaded subject must re-bake; the planes did not move at all");

    let oracle = regions::rasterize(&posed, &scores, &settings(), eye, &view_proj, CANVAS_WIDTH, CANVAS_HEIGHT);
    assert_parity("posed", &oracle, &after, CANVAS_WIDTH, CANVAS_HEIGHT);
}

/// The shipped subject, at the size and framing the easel develops at.
const CROSSFEED_WIDTH: usize = 900;
const CROSSFEED_HEIGHT: usize = 1200;

/// How many bakes the cost measurement averages over, after a warm-up
/// that pays for the geometry's first realization on the GPU.
const COST_SAMPLES: u32 = 20;

/// Cross-feed instrument: the GPU bake against the CPU oracle on the
/// real subject, reporting drift and the bake's added per-frame cost.
///
/// Ignored by default — it needs a 434k-face mesh and its material
/// field, which live outside the repository. See the module header for
/// how to drive it.
#[test]
#[ignore = "instrument; needs the shipped subject in AETHER_CROSSFEED_DIR"]
fn crossfeed_the_gpu_bake_against_the_cpu_oracle() {
    if !require_wgpu_only() {
        return;
    }
    let Ok(dir) = env::var("AETHER_CROSSFEED_DIR") else {
        eprintln!("AETHER_CROSSFEED_DIR unset; nothing to cross-feed");
        return;
    };
    let dir = Path::new(&dir);

    let mesh = Mesh::from_obj_bytes(&fs::read(dir.join("subject.obj")).expect("read subject.obj"), 0)
        .expect("parse the subject");
    let labels =
        Labels::parse(&fs::read(dir.join("labels.npy")).expect("read labels.npy"), mesh.min, mesh.max, LABEL_PAD)
            .expect("parse the material field");
    let scores = labels.vertex_scores(&mesh.positions);
    let (eye, view_proj) = camera(AZIMUTH, CROSSFEED_WIDTH, CROSSFEED_HEIGHT);

    let started = Instant::now();
    let oracle = regions::rasterize(&mesh, &scores, &settings(), eye, &view_proj, CROSSFEED_WIDTH, CROSSFEED_HEIGHT);
    let oracle_millis = started.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "crossfeed: {} faces, {} vertices; the CPU oracle baked in {oracle_millis:.1} ms",
        mesh.faces.len(),
        mesh.positions.len(),
    );

    let mut harness = SubstrateHarness::builder()
        .size(CROSSFEED_WIDTH as u32, CROSSFEED_HEIGHT as u32)
        .with_render()
        .build()
        .expect("harness");
    let rig = Rig::mount(&mut harness, &mesh, &scores, CROSSFEED_WIDTH, CROSSFEED_HEIGHT);

    // The two costs are measured in separate blocks, bare first, and
    // never interleaved.
    //
    // Interleaving them — one bare capture after each dispatch frame, to
    // difference the two — reads almost zero however expensive the bake
    // is, and the reason is worth stating because the shape of it recurs
    // anywhere a GPU is timed from the CPU. A capture blocks on the
    // device queue, so the bare capture that follows a dispatch waits out
    // the dispatch's own work: measured here, a bare capture costs 6 ms
    // cold and 58 ms taken straight after a dispatch frame that itself
    // costs 60 ms. Differencing those two reports a couple of
    // milliseconds — noise between two nearly equal numbers — for work
    // that takes tens. The bare block therefore runs first, before any
    // dispatch has been issued, and the difference below is the honest
    // one.
    let mut bare = 0.0;
    for _ in 0..COST_SAMPLES {
        let started = Instant::now();
        harness.execute(vec![("bare", HarnessOp::capture())]).expect("bare capture");
        bare += started.elapsed().as_secs_f64() * 1000.0;
    }
    let bare_millis = bare / f64::from(COST_SAMPLES);

    // Warm: the first use realizes the vertex and index buffers, which
    // is a geometry-upload cost and not a per-frame one.
    let baked = rig.read_planes(&mut harness, eye, view_proj);

    let mut dispatched = 0.0;
    for _ in 0..COST_SAMPLES {
        let started = Instant::now();
        rig.read_planes(&mut harness, eye, view_proj);
        dispatched += started.elapsed().as_secs_f64() * 1000.0;
    }
    let dispatched_millis = dispatched / f64::from(COST_SAMPLES);

    let worst = assert_parity("crossfeed", &oracle, &baked, CROSSFEED_WIDTH, CROSSFEED_HEIGHT);
    eprintln!(
        "crossfeed: worst level-0.5 drift {worst:.3} px over every class; a bake frame costs \
         {dispatched_millis:.2} ms at {CROSSFEED_WIDTH}x{CROSSFEED_HEIGHT} against a {bare_millis:.2} ms bare \
         capture — {:.2} ms for the bake, against a {oracle_millis:.1} ms CPU bake",
        dispatched_millis - bare_millis,
    );
}
