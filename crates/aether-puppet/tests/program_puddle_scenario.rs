//! GPU-versus-CPU parity for the puddle ops (iamacoffeepot/aether#4366,
//! ADR-0170): each authored pass in `easel/program/puddle.wgsl` driven
//! standalone through the `aether.render.program` mail surface against its
//! CPU counterpart in `easel::image` / `easel::field` on identical inputs.
//!
//! The comparison space, stated honestly: an op develops into an `Rgba8`
//! writable binding (one round-to-8-bit step), the overlay path draws that
//! texture texel-for-pixel into the sRGB offscreen target (an sRGB encode
//! plus a second rounding), and the capture returns those bytes. The
//! assertions decode the PNG byte back through the inverse sRGB transfer
//! into linear space and compare there, where one 8-bit plane step is
//! `1/255` everywhere. The budget per pixel: op math is a transcription
//! (float-association drift well under a step), the `Rgba8Unorm` write
//! rounds within half a step, and the sRGB encode-decode round trip costs
//! at most about one more step where the transfer curve is shallowest — so
//! identical math lands within two steps, and the threshold is three.
//! Wrong math — a mis-sized tap window, a flipped clamp, a mis-strided
//! noise window — shifts whole neighbourhoods by many steps.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]
// Plane indexing math casts between pixel indices and `f32` coordinates
// the same way the easel crate itself does (see its crate-level rationale),
// every count bounded by the tiny test plane; and the oracle
// transcriptions must stay textually identical to the CPU formulas they
// mirror, so no `mul_add` rewrites.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, rgba_at, srgb_byte_to_linear};
use aether_harness_substrate_capture::visual::decode_png;
use aether_kinds::QuadSpace;
use aether_math::{Rgba, Vec2};
use aether_puppet::easel::image;
use aether_puppet::easel::program::puddle::{
    BLUR_PASSES, BoxBlurChain, BoxBlurUniforms, EDGE_BAND, MAX_FUSED_WEIGHTS, PUDDLE_WGSL, RIM_RESTRIDE, RIM_VARY,
    RIM_VARY_CEILING, RimUniforms, ShrinkUniforms, ThresholdUniforms, box_blur_passes, box_half_width, plane_slot,
    reduced_plane_slot, rim_pass, shrink_pass, soft_carry_pass, soft_plane_slot, threshold_pass,
};
use aether_puppet::math3::hash_unit;
use aether_render::QuadBlend;
use aether_render::{
    CreateTexture, CreateTextureResult, DrawTexturedQuads, InputSlot, OutputSlot, ProgramDispatch, ProgramPass,
    ProgramRegister, ProgramRegisterResult, SlotExtent, SlotSpec, TextureFormat, TextureSampling, TextureUsage,
    TexturedQuad,
};

/// Plane extent every scenario develops at: small enough that the CPU
/// oracle is instant, large enough that blur windows, wrap-around noise
/// windows, and off-plane resamples all genuinely occur.
const PLANE_WIDTH: usize = 48;
const PLANE_HEIGHT: usize = 32;

/// Per-pixel budget in linear 8-bit plane steps (see the module doc for
/// the accounting). Identical math stays within two; three leaves one
/// step of slack without admitting any wrong-window bug, which shifts
/// pixels by tens of steps.
const TOLERANCE_STEPS: f32 = 3.0;

/// Budget for the reduced-extent chain, which softens by a little more
/// than the oracle by construction: the block average and the bilinear
/// carry-back add their own support to the window, and the carry-back
/// resolves the reduced plane's curve as straight lines between its
/// samples. Both are fractions of a step across the interior of a field
/// this smooth. The plane's mirrored border is where they are not: the
/// sweep there folds the reduced plane, whose last texels are already
/// averages of the last few of the oracle's, so the two extend their
/// edges differently and the border rows carry the whole budget. The
/// mechanism bugs named below move whole neighbourhoods by tens of steps.
const REDUCED_TOLERANCE_STEPS: f32 = 8.0;

/// The reduction [`a_reduced_extent_blur_chain_matches_cpu_blur`] sweeps
/// at.
const DIVISOR: u32 = 2;

/// Skip (or panic under `AETHER_REQUIRE_RUNTIME`) when no wgpu adapter
/// is available — every scenario here executes programs for real.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

fn boot() -> SubstrateHarness {
    SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot")
}

/// A register mail over the puddle module: `input_count` `R32Float`
/// plane bindings, then the `Rgba8` writable output binding the scenarios
/// observe, plus `transient_count` plane transients.
fn plane_program(input_count: usize, transient_count: usize, passes: Vec<ProgramPass>) -> ProgramRegister {
    let mut bindings = vec![plane_slot(); input_count];
    bindings.push(SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full });

    ProgramRegister {
        wgsl: PUDDLE_WGSL.to_owned(),
        bindings,
        transients: vec![plane_slot(); transient_count],
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes,
    }
}

/// The output binding's index under [`plane_program`]'s layout.
fn output_binding(input_count: usize) -> u32 {
    u32::try_from(input_count).expect("input count fits u32")
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

/// Upload one CPU plane as the `R32Float`, nearest-bound data texture the
/// puddle ops read.
fn create_plane(harness: &mut SubstrateHarness, label: &'static str, plane: &[f32]) -> u32 {
    create_texture(
        harness,
        label,
        &CreateTexture {
            width: PLANE_WIDTH as u32,
            height: PLANE_HEIGHT as u32,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: plane.iter().flat_map(|value| value.to_le_bytes()).collect(),
        },
    )
}

/// Run one registered puddle program over CPU-uploaded input planes and
/// return the developed output plane, decoded from the capture back into
/// linear space: dispatch, draw the output binding texel-for-pixel
/// through the overlay path, capture, and invert the target's sRGB
/// transfer per pixel.
fn develop(
    harness: &mut SubstrateHarness,
    register: &ProgramRegister,
    inputs: &[&[f32]],
    uniforms: Vec<u8>,
) -> Vec<f32> {
    const INPUT_LABELS: [&str; 3] = ["create_plane_0", "create_plane_1", "create_plane_2"];
    assert!(inputs.len() <= INPUT_LABELS.len(), "widen INPUT_LABELS for a program with more input planes");
    let mut bindings: Vec<u32> =
        inputs.iter().zip(INPUT_LABELS).map(|(plane, label)| create_plane(harness, label, plane)).collect();
    bindings.push(create_texture(
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
    ));
    let output_id = *bindings.last().expect("output binding just pushed");

    let program_id = match harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", register))])
        .expect("register sequence")
        .reply::<ProgramRegisterResult>("register")
        .expect("decode ProgramRegisterResult")
    {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    };

    // The output texture drawn texel-for-pixel at the window's top-left:
    // pixel centers land on texel centers, so the linear sampler returns
    // each texel exactly.
    let overlay = DrawTexturedQuads {
        texture_id: output_id,
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
    let pre = vec![
        envelope("aether.render", &ProgramDispatch { program_id, bindings, geometries: Vec::new(), uniforms }),
        envelope("aether.render", &overlay),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture program output");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

    let mut plane = Vec::with_capacity(PLANE_WIDTH * PLANE_HEIGHT);
    for y in 0..PLANE_HEIGHT {
        for x in 0..PLANE_WIDTH {
            plane.push(srgb_byte_to_linear(rgba_at(&img, x as u32, y as u32)[0]));
        }
    }
    plane
}

/// Every pixel of the developed plane within [`TOLERANCE_STEPS`] of the
/// CPU oracle (clamped to the displayable `[0, 1]` the readback path can
/// carry).
fn assert_plane_close(op: &str, developed: &[f32], expected: &[f32]) {
    assert_plane_within(op, developed, expected, TOLERANCE_STEPS);
}

/// [`assert_plane_close`] against a budget the op states for itself — the
/// reduced-extent chain, whose plane genuinely carries fewer texels than
/// the oracle's.
fn assert_plane_within(op: &str, developed: &[f32], expected: &[f32], budget: f32) {
    assert_eq!(developed.len(), expected.len(), "{op}: plane sizes must agree");
    let mut worst = 0.0f32;
    let mut worst_at = (0, 0);

    for (index, (&got, &want)) in developed.iter().zip(expected).enumerate() {
        let steps = (got - want.clamp(0.0, 1.0)).abs() * 255.0;
        if steps > worst {
            worst = steps;
            worst_at = (index % PLANE_WIDTH, index / PLANE_WIDTH);
        }
    }
    assert!(
        worst <= budget,
        "{op}: GPU develop drifts {worst:.2} linear 8-bit steps from the CPU oracle at {worst_at:?} \
         (budget {budget})",
    );
}

/// A soft disc of coverage: one inside `hold`, zero past `give`, a
/// hermite ramp between — a region mask with a gradient band, so the
/// resample and threshold scenarios exercise fractional values rather
/// than a binary silhouette.
fn disc_mask(centre: Vec2, hold: f32, give: f32) -> Vec<f32> {
    let mut mask = Vec::with_capacity(PLANE_WIDTH * PLANE_HEIGHT);
    for y in 0..PLANE_HEIGHT {
        for x in 0..PLANE_WIDTH {
            let reach = Vec2::new(x as f32 - centre.x, y as f32 - centre.y).length();
            mask.push(image::smoothstep(give, hold, reach));
        }
    }
    mask
}

/// A deterministic stand-in for the sheet's tide-line noise, spanning
/// roughly `[-0.7, 0.7)` like the real `EDGE_MIX` blend, so the rim's
/// vary clamp is exercised at both ends.
fn edge_noise(salt: u64) -> Vec<f32> {
    (0..PLANE_WIDTH * PLANE_HEIGHT).map(|i| (hash_unit(i as u64 ^ salt) - 0.5) * 1.4).collect()
}

/// The fused blur chain against `image::blur` on a pseudo-random field.
/// Two authored sweeps — the chain's three iterations convolved into one
/// kernel per axis (iamacoffeepot/aether#4441) — must land the softening
/// the CPU's three landed, over the whole plane and not merely inside it:
/// the plane's mirrored edge (iamacoffeepot/aether#4444) is what makes
/// re-extending between three sweeps land where extending once does, so
/// the border rows are held to the same budget as the interior.
///
/// The named bugs: a tap window sized `2r` or `2r + 2` (an off-by-one
/// either side of a sweep's loop bounds), an edge folded one texel short
/// (the border rows drift while the interior stays plausible), the
/// horizontal and vertical sweeps reading the same axis so anisotropy
/// sneaks into a square blur, and a kernel convolved from the wrong
/// iteration count (two or four box passes read as a blur but land whole
/// neighbourhoods off the three-pass oracle).
#[test]
fn box_blur_chain_matches_cpu_blur() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    let field: Vec<f32> = (0..PLANE_WIDTH * PLANE_HEIGHT).map(|i| hash_unit(i as u64)).collect();
    let radius_pixels = 5.1;
    let expected = image::blur(&field, PLANE_WIDTH, PLANE_HEIGHT, radius_pixels);

    let half_width_texels = box_half_width(radius_pixels, 1);
    let chain = BoxBlurChain { scratch: 0, carry: 1, divisor: 1, half_width_texels };
    // A fused sweep pairs its taps through a filtering sampler, so the
    // plane it reads has to stand at the soft format; the field arrives
    // here as a staged 32-bit binding, so it is carried onto one first,
    // exactly as the wash's own graph carries the ink plane.
    let mut passes = vec![soft_carry_pass(InputSlot::Binding { index: 0 }, OutputSlot::Transient { index: 2 })];
    passes.extend(box_blur_passes(
        InputSlot::Transient { index: 2 },
        &chain,
        OutputSlot::Binding { index: output_binding(1) },
        0,
    ));
    let mut register = plane_program(1, 3, passes);
    register.transients = vec![soft_plane_slot(); 3];
    assert_eq!(register.passes.len(), 3, "a chain this narrow fuses to one sweep per axis, over the carried plane");
    let uniforms = BoxBlurUniforms { half_width_texels, divisor: chain.divisor }.encode().to_vec();
    let developed = develop(&mut harness, &register, &[&field], uniforms);

    assert_plane_close("box blur", &developed, &expected);
}

/// A chain whose composite outruns the weights the uniform block carries
/// keeps its [`BLUR_PASSES`] iterations, and those are the six sweeps
/// they always were — bit-for-bit the CPU running sum, border included.
/// The named bug: a fused sweep laid anyway over a kernel the block
/// truncates, which softens by whatever fraction of the window survived.
#[test]
fn a_blur_too_wide_to_fuse_keeps_its_iterations() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    let field = disc_mask(Vec2::new(23.0, 15.0), 6.0, 20.0);
    // Past the point where three sweeps of this reach convolve to more
    // weights than the block declares.
    let radius_pixels = 1.7 * (MAX_FUSED_WEIGHTS / BLUR_PASSES as usize + 1) as f32;
    let expected = image::blur(&field, PLANE_WIDTH, PLANE_HEIGHT, radius_pixels);

    let half_width_texels = box_half_width(radius_pixels, 1);
    let chain = BoxBlurChain { scratch: 0, carry: 1, divisor: 1, half_width_texels };
    let register = plane_program(
        1,
        2,
        box_blur_passes(InputSlot::Binding { index: 0 }, &chain, OutputSlot::Binding { index: output_binding(1) }, 0),
    );
    assert_eq!(register.passes.len(), 2 * BLUR_PASSES as usize, "a chain this wide keeps its iterations");
    let uniforms = BoxBlurUniforms { half_width_texels, divisor: chain.divisor }.encode().to_vec();
    let developed = develop(&mut harness, &register, &[&field], uniforms);

    assert_plane_close("unfused box blur", &developed, &expected);
}

/// The reduced-extent blur chain against the same CPU oracle
/// (iamacoffeepot/aether#4437): a chain that sweeps a plane half as wide
/// on each axis, opened by the block-average downsample and closed by the
/// bilinear upsample, must still land the softening the full-extent chain
/// lands. The radius here halves exactly, so the reduction costs the
/// oracle nothing it can be held to — what is left to get wrong is the
/// mechanism. The named bugs: a downsample reading the reduced texel's
/// own coordinate rather than its block (the softening lands a quarter of
/// the plane away), an upsample sampling corners instead of texel centres
/// (a half-texel shift the whole plane carries), one that drops the
/// bilinear weights for a nearest read (the reduced texels show as
/// blocks), and a sweep window still sized for the full extent (double
/// the softening asked for).
#[test]
fn a_reduced_extent_blur_chain_matches_cpu_blur() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    // Smooth by construction, as every plane the wash blurs is: the
    // reduction discards frequencies above its own extent, so a
    // pseudo-random field would measure the discarding rather than the
    // chain. Twice BOX_TO_GAUSSIAN times MIN_REDUCED_RADIUS, so both
    // extents round to a whole box radius and neither side rounds away
    // from the other.
    let field = disc_mask(Vec2::new(23.0, 15.0), 6.0, 20.0);
    let radius_pixels = 13.6;
    let expected = image::blur(&field, PLANE_WIDTH, PLANE_HEIGHT, radius_pixels);

    let half_width_texels = box_half_width(radius_pixels, DIVISOR);
    let chain = BoxBlurChain { scratch: 0, carry: 1, divisor: DIVISOR, half_width_texels };
    let mut register = plane_program(
        1,
        2,
        box_blur_passes(InputSlot::Binding { index: 0 }, &chain, OutputSlot::Binding { index: output_binding(1) }, 0),
    );
    register.transients = vec![reduced_plane_slot(DIVISOR); 2];
    let uniforms = BoxBlurUniforms { half_width_texels, divisor: DIVISOR }.encode().to_vec();
    let developed = develop(&mut harness, &register, &[&field], uniforms);

    assert_plane_within("reduced box blur", &developed, &expected, REDUCED_TOLERANCE_STEPS);
}

/// The scale-about-centroid resample against a transcription of
/// `field::shrink` over `image::sample_bilinear`. The pour must land
/// smaller about the wash's centre and displaced by the jitter, with
/// everything read from off the plane counting as zero. The named bugs:
/// an inverted scale (multiplying by `scale` instead of dividing grows
/// the pour), a flipped jitter sign (the pour wanders the wrong way), a
/// bilinear that clamps at the plane's edge instead of reading zero (the
/// border smears inward), and swapped axes in the source coordinate.
#[test]
fn shrink_resample_matches_cpu_shrink() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    let centre = Vec2::new(23.0, 15.0);
    let mask = disc_mask(centre, 9.0, 14.0);
    let jitter = Vec2::new(3.7, -2.4);
    let scale = 0.8;
    let mut expected = Vec::with_capacity(mask.len());
    for y in 0..PLANE_HEIGHT {
        let source_y = centre.y + (y as f32 - centre.y - jitter.y) / scale;
        for x in 0..PLANE_WIDTH {
            let source_x = centre.x + (x as f32 - centre.x - jitter.x) / scale;
            expected.push(image::sample_bilinear(&mask, PLANE_WIDTH, PLANE_HEIGHT, source_x, source_y));
        }
    }

    let register = plane_program(
        1,
        0,
        vec![shrink_pass(InputSlot::Binding { index: 0 }, OutputSlot::Binding { index: output_binding(1) }, 0)],
    );
    let uniforms = ShrinkUniforms { centre, jitter, scale }.encode().to_vec();
    let developed = develop(&mut harness, &register, &[&mask], uniforms);

    assert_plane_close("shrink", &developed, &expected);
}

/// The threshold band against a transcription of `Sheet::threshold`'s
/// hard edge over `image::smoothstep`: the hermite ramp across the
/// softened puddle, its edges shifted by the tide-line noise read at the
/// pour's displaced window — which wraps on both axes here, since the
/// offsets exceed what the plane extent leaves at the far corner. The
/// named bugs: a mis-strided noise window (offsets unapplied, swapped
/// across axes, or read without the wrap — every tide line lands on the
/// wrong stretch of noise), swapped hermite edges (the band inverts), and
/// wobble scaling the puddle value instead of displacing the band.
#[test]
fn threshold_band_matches_cpu_threshold() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    let soft = image::blur(&disc_mask(Vec2::new(23.0, 15.0), 9.0, 14.0), PLANE_WIDTH, PLANE_HEIGHT, 6.0);
    let noise = edge_noise(0xed_9e);
    let window = (17u32, 29u32);
    let (level, wobble) = (0.38, 0.55);
    let mut expected = Vec::with_capacity(soft.len());
    for y in 0..PLANE_HEIGHT {
        let noise_row = ((y + window.1 as usize) % PLANE_HEIGHT) * PLANE_WIDTH;
        for x in 0..PLANE_WIDTH {
            let shift = noise[noise_row + (x + window.0 as usize) % PLANE_WIDTH] * wobble;
            expected.push(image::smoothstep(
                level - EDGE_BAND + shift,
                level + EDGE_BAND + shift,
                soft[y * PLANE_WIDTH + x],
            ));
        }
    }

    let register = plane_program(
        2,
        0,
        vec![threshold_pass(
            InputSlot::Binding { index: 0 },
            InputSlot::Binding { index: 1 },
            OutputSlot::Binding { index: output_binding(2) },
            0,
        )],
    );
    let uniforms = ThresholdUniforms { window, level, band: EDGE_BAND, wobble }.encode().to_vec();
    let developed = develop(&mut harness, &register, &[&soft, &noise], uniforms);

    assert_plane_close("threshold", &developed, &expected);
}

/// The rim against a transcription of the rim block inside `Sheet::pour`:
/// alpha minus its blurred interior, floored at zero, varied by the edge
/// noise read at the restrided window and clamped at the vary ceiling.
/// The inputs guarantee the clamp genuinely bites at both ends where rim
/// pigment exists (asserted below), so a shader that drops the ceiling or
/// the zero floor cannot pass. The named bugs: an unclamped vary (the
/// tide line darkens past the ceiling), a missing negative-rim floor (the
/// interior side of the band goes negative and drags the develop down),
/// and the restride dropped or misapplied so the rim's strength varies
/// along the same noise stretch that placed the edge.
#[test]
fn rim_matches_cpu_pour_rim_block() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    let alpha = disc_mask(Vec2::new(23.0, 15.0), 10.0, 12.0);
    let interior = image::blur(&alpha, PLANE_WIDTH, PLANE_HEIGHT, 7.0);
    let noise = edge_noise(0x71_de);
    let window = (5u32, 3u32);
    let strength = 1.0;

    let (mut ceiling_bites, mut floor_bites) = (0u32, 0u32);
    let mut expected = Vec::with_capacity(alpha.len());
    for y in 0..PLANE_HEIGHT {
        let noise_row = ((y + window.1 as usize * RIM_RESTRIDE.1) % PLANE_HEIGHT) * PLANE_WIDTH;
        for x in 0..PLANE_WIDTH {
            let rim = (alpha[y * PLANE_WIDTH + x] - interior[y * PLANE_WIDTH + x]).max(0.0);
            let raw =
                RIM_VARY.0 + noise[noise_row + (x + window.0 as usize * RIM_RESTRIDE.0) % PLANE_WIDTH] * RIM_VARY.1;
            if rim > 0.05 && raw > RIM_VARY_CEILING {
                ceiling_bites += 1;
            }
            if rim > 0.05 && raw < 0.0 {
                floor_bites += 1;
            }
            expected.push(rim * raw.clamp(0.0, RIM_VARY_CEILING) * strength);
        }
    }
    assert!(
        ceiling_bites > 0 && floor_bites > 0,
        "test inputs must exercise both vary clamp ends over live rim pigment; got ceiling {ceiling_bites}, \
         floor {floor_bites}",
    );

    let register = plane_program(
        3,
        0,
        vec![rim_pass(
            InputSlot::Binding { index: 0 },
            InputSlot::Binding { index: 1 },
            InputSlot::Binding { index: 2 },
            OutputSlot::Binding { index: output_binding(3) },
            0,
        )],
    );
    let uniforms = RimUniforms { window, strength }.encode().to_vec();
    let developed = develop(&mut harness, &register, &[&alpha, &interior, &noise], uniforms);

    assert_plane_close("rim", &developed, &expected);
}
