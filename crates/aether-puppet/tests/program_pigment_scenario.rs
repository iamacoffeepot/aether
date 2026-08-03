//! Parity scenarios for the pigment ops as authored passes (ADR-0170,
//! iamacoffeepot/aether#4367): each op registered through the
//! `aether.render.program` mail surface, driven over CPU-computed input
//! planes, and held against its CPU oracle on identical inputs.
//!
//! The comparison space, honestly: each program develops into an
//! `R32Float` transient at full f32 precision, and a test-only `fs_show`
//! pass packs that into the `Rgba8` output binding (the overlay path
//! refuses non-filterable formats, so this is the established readback
//! route). The capture then stores through the harness' sRGB offscreen
//! target, so each observed byte is `srgb(quantize(value))`; the
//! comparisons decode that back to linear and hold it against the
//! oracle's f32 within a stated tolerance. The budget behind the
//! tolerances: the `Rgba8Unorm` pack quantizes to half a step (0.5/255),
//! the sRGB store-and-decode round trip costs up to ~1.2 further linear
//! steps at the bright end, and the op arithmetic itself is ported
//! statement-for-statement — its divergence is measured in f32 ulps —
//! except where a note below says otherwise (WGSL's `cos`/`sin`
//! tolerance for spatter, rounding-boundary tap swaps for the smear).
//! Every genuine porting bug these scenarios name moves pixels by tens
//! of 8-bit steps, so the thresholds keep two orders of discrimination.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]
// The oracle mirrors restate field.rs geometry, and answer the numeric
// cast lints the way the crate itself does.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, rgba_at};
use aether_harness_substrate_capture::visual::{Image, decode_png};
use aether_kinds::QuadSpace;
use aether_math::{Rgba, Vec2};
use aether_puppet::easel::field::DropAccident;
use aether_puppet::easel::image::{Flow, Noise, smear_along_flow};
use aether_puppet::easel::program::pigment::{
    GranulateUniforms, PIGMENT_WGSL, SMEAR_PASSES, SagUniforms, SmearSlots, SmearUniforms, SpatterUniforms,
    granulate_pass, plane_slot, sag_pass, smear_passes, spatter_pass,
};
use aether_render::{
    CreateTexture, CreateTextureResult, DrawTexturedQuads, InputSlot, OutputSlot, PassStage, ProgramDispatch,
    ProgramPass, ProgramRegister, ProgramRegisterResult, SlotExtent, SlotSpec, TextureFormat, TextureSampling,
    TextureUsage, TexturedQuad,
};

/// Canvas the planes are developed at. Small enough that llvmpipe runs
/// each program in milliseconds, large enough that every op's geometry —
/// sag steps, drop discs, advection segments — has room to act.
const CANVAS_WIDTH: usize = 96;
const CANVAS_HEIGHT: usize = 64;

/// Where the readback overlay quad sits in the window, which is sized to
/// hold it with a margin.
const QUAD_ORIGIN: (u32, u32) = (16, 16);

/// Tolerance for the ops ported statement-for-statement (granulation,
/// sag): quantize half-step plus the sRGB round trip, doubled for
/// margin.
const TOLERANCE_REPLICATED: f32 = 3.0 / 255.0;

/// Tolerance for the smear: the replicated budget plus rounding-boundary
/// tap swaps — a fused multiply-add in the driver can move an advection
/// sample position across a half-texel boundary by an ulp, swapping one
/// tap of the segment average for its neighbour, which over a smooth
/// field moves the result by under two further steps.
const TOLERANCE_SMEAR: f32 = 4.0 / 255.0;

/// Tolerance for spatter: the replicated budget plus WGSL's `cos`/`sin`
/// accuracy (absolute error up to 2^-11), which at the fixture's throws
/// shifts a landing by up to ~0.02 texels and a disc-slope texel by
/// about one further step.
const TOLERANCE_TRIG: f32 = 5.0 / 255.0;

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

/// Test-only tail concatenated onto [`PIGMENT_WGSL`]: packs the
/// developed `R32Float` plane into the `Rgba8` output binding so the
/// overlay path can sample it.
const SHOW_WGSL: &str = r"
@group(1) @binding(0) var shown_plane: texture_2d<f32>;

@fragment
fn fs_show(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let value = textureLoad(shown_plane, vec2<i32>(position.xy), 0).r;
    return vec4<f32>(value, value, value, 1.0);
}
";

/// The show pass: developed transient in, `Rgba8` dispatch binding out.
fn show_pass(shown: u32, output_binding: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_show".to_owned(),
        inputs: vec![InputSlot::Transient { index: shown }],
        output: OutputSlot::Binding { index: output_binding },
        uniform_offset: 0,
        uniform_length: 0,
        repeat: None,
    }
}

fn boot() -> SubstrateHarness {
    SubstrateHarness::builder().size(128, 96).with_render().build().expect("boot substrate harness")
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

/// Upload one canvas-sized f32 plane as an `R32Float` registry texture —
/// bit-exact staging, so the GPU op reads the very floats the oracle
/// read.
fn create_plane(harness: &mut SubstrateHarness, label: &'static str, plane: &[f32]) -> u32 {
    create_texture(
        harness,
        label,
        &CreateTexture {
            width: CANVAS_WIDTH as u32,
            height: CANVAS_HEIGHT as u32,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: plane.iter().flat_map(|value| value.to_le_bytes()).collect(),
        },
    )
}

/// The writable `Rgba8` texture `fs_show` packs into — the program's
/// declared result binding.
fn create_shown_output(harness: &mut SubstrateHarness) -> u32 {
    create_texture(
        harness,
        "create_shown_output",
        &CreateTexture {
            width: CANVAS_WIDTH as u32,
            height: CANVAS_HEIGHT as u32,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        },
    )
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

/// Dispatch the program, draw its shown output 1:1 through the overlay
/// path, capture, and decode the quad region back into a linear plane.
fn developed_plane(
    harness: &mut SubstrateHarness,
    program_id: u32,
    bindings: Vec<u32>,
    uniforms: Vec<u8>,
    shown_output_id: u32,
) -> Vec<f32> {
    let overlay = DrawTexturedQuads {
        texture_id: shown_output_id,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![TexturedQuad {
            x: QUAD_ORIGIN.0 as f32,
            y: QUAD_ORIGIN.1 as f32,
            width: CANVAS_WIDTH as f32,
            height: CANVAS_HEIGHT as f32,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    };
    let pre = vec![
        envelope("aether.render", &ProgramDispatch { program_id, bindings, uniforms }),
        envelope("aether.render", &overlay),
    ];

    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture program output");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

    canvas_pixels(&img)
}

/// The quad region of a captured frame, decoded from the sRGB target's
/// bytes back to the linear values the show pass packed.
fn canvas_pixels(img: &Image) -> Vec<f32> {
    let mut linear = Vec::with_capacity(CANVAS_WIDTH * CANVAS_HEIGHT);
    for y in 0..CANVAS_HEIGHT {
        for x in 0..CANVAS_WIDTH {
            linear.push(srgb_to_linear(rgba_at(img, QUAD_ORIGIN.0 + x as u32, QUAD_ORIGIN.1 + y as u32)[0]));
        }
    }
    linear
}

/// Invert the offscreen target's sRGB transfer, byte to linear.
fn srgb_to_linear(byte: u8) -> f32 {
    let encoded = f32::from(byte) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Hold the observed linear plane against the oracle's, reporting the
/// worst texel. The oracle side clamps to `[0, 1]` because the `Rgba8`
/// pack does — the fixtures keep their values under one, so the clamp is
/// a guard rather than a participant.
fn assert_plane_close(op: &str, expected: &[f32], observed: &[f32], tolerance: f32) {
    assert_eq!(expected.len(), observed.len(), "{op}: plane sizes must match");

    let mut worst = (0usize, 0.0f32);
    for (i, (&want, &got)) in expected.iter().zip(observed).enumerate() {
        let diff = (want.clamp(0.0, 1.0) - got).abs();
        if diff > worst.1 {
            worst = (i, diff);
        }
    }
    let (i, diff) = worst;
    assert!(
        diff <= tolerance,
        "{op}: worst difference {diff} at ({}, {}) — oracle {} observed {} (tolerance {tolerance})",
        i % CANVAS_WIDTH,
        i / CANVAS_WIDTH,
        expected[i],
        observed[i],
    );
}

fn canvas_index(x: usize, y: usize) -> usize {
    y * CANVAS_WIDTH + x
}

/// Mirror of field.rs `Sheet::granulate` over explicit planes (the
/// method is private to the wash). Constants restate GRANULATION_FLOOR /
/// _AUTHORITY / _PIVOT; if field.rs moves them, the sequencer's
/// whole-wash parity scenario (iamacoffeepot/aether#4369) is the
/// tripwire that catches the shader lagging.
fn granulated_oracle(density: &[f32], tooth: &[f32], gran: f32) -> Vec<f32> {
    density
        .iter()
        .zip(tooth)
        .map(|(&at, &grain)| {
            if at > 0.003 {
                at * (1.0 - gran * 0.85 * (grain - 0.18))
            } else {
                at
            }
        })
        .collect()
}

/// Mirror of field.rs `sagged` with the step explicit (the free
/// function is private and derives its step from the canvas height;
/// `SagUniforms::for_canvas` replicates that derivation, and the
/// scenario passes the step directly so it stays meaningful at this
/// small canvas).
fn sagged_oracle(soft: &[f32], step: usize) -> Vec<f32> {
    let mut out = soft.to_vec();

    for y in 0..CANVAS_HEIGHT {
        for x in 0..CANVAS_WIDTH {
            let i = canvas_index(x, y);
            for (drop, carried) in [0.8f32, 0.55].iter().enumerate() {
                let above = (drop + 1) * step;
                if y >= above {
                    out[i] = out[i].max(soft[i - above * CANVAS_WIDTH] * carried);
                }
            }
        }
    }

    out
}

/// Mirror of field.rs `Sheet::spatter` (the method is private to the
/// wash), bounding boxes, clamps and all. 1.25 restates SPATTER_DROOP.
fn spattered_oracle(density: &[f32], centre: Vec2, drops: &[DropAccident]) -> Vec<f32> {
    let mut out = density.to_vec();

    for drop in drops {
        let at =
            Vec2::new(centre.x + drop.bearing.cos() * drop.throw, centre.y + drop.bearing.sin() * drop.throw * 1.25);

        let x0 = (at.x - drop.radius - 1.0).max(0.0) as usize;
        let x1 = ((at.x + drop.radius + 1.0) as usize).min(CANVAS_WIDTH - 1);
        let y0 = (at.y - drop.radius - 1.0).max(0.0) as usize;
        let y1 = ((at.y + drop.radius + 1.0) as usize).min(CANVAS_HEIGHT - 1);

        for y in y0..=y1 {
            for x in x0..=x1 {
                let reach = Vec2::new(x as f32 - at.x, y as f32 - at.y).length();
                if reach < drop.radius {
                    out[canvas_index(x, y)] += drop.strength * (1.0 - reach / drop.radius);
                }
            }
        }
    }

    out
}

/// A smooth radial density blob whose skirt falls through the
/// granulation floor to zero — both branches of the floor guard live in
/// one plane.
fn blob_plane() -> Vec<f32> {
    let centre = Vec2::new(48.0, 32.0);
    (0..CANVAS_WIDTH * CANVAS_HEIGHT)
        .map(|i| {
            let at = Vec2::new((i % CANVAS_WIDTH) as f32, (i / CANVAS_WIDTH) as f32);
            (1.0 - (at - centre).length() / 40.0).max(0.0) * 0.85
        })
        .collect()
}

/// Granulation parity: the tooth-keyed pivot applied over a real noise
/// tooth plane. The plausible bugs this catches: the pivot's sense
/// inverted (density modulated up where the grain should lift it off the
/// peaks — every toothy texel lands tens of steps off), the floor guard
/// lost (the blob's sub-floor skirt granulating bare paper), and the
/// strength window mis-sliced (a wrong `gran` scales the whole
/// modulation).
#[test]
fn granulate_program_matches_the_cpu_oracle() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    let density = blob_plane();
    let tooth = Noise::new(0xa1_f0, 4, 24.0).plane(CANVAS_WIDTH, CANVAS_HEIGHT);
    let gran = 0.7;

    let density_id = create_plane(&mut harness, "create_density", &density);
    let tooth_id = create_plane(&mut harness, "create_tooth", &tooth);
    let shown_id = create_shown_output(&mut harness);
    let program_id = register(
        &mut harness,
        &ProgramRegister {
            wgsl: format!("{PIGMENT_WGSL}\n{SHOW_WGSL}"),
            bindings: vec![
                plane_slot(),
                plane_slot(),
                SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full },
            ],
            transients: vec![plane_slot()],
            passes: vec![
                granulate_pass(
                    InputSlot::Binding { index: 0 },
                    InputSlot::Binding { index: 1 },
                    OutputSlot::Transient { index: 0 },
                    0,
                ),
                show_pass(0, 2),
            ],
        },
    );

    let observed = developed_plane(
        &mut harness,
        program_id,
        vec![density_id, tooth_id, shown_id],
        GranulateUniforms { gran }.encode(),
        shown_id,
    );
    assert_plane_close("granulate", &granulated_oracle(&density, &tooth, gran), &observed, TOLERANCE_REPLICATED);
}

/// Sag parity: the two downhill drag samples at their authored spacing
/// and carry weights. The fixture is a horizontal band high on the
/// canvas, so the drag direction is unambiguous. The plausible bugs this
/// catches: the downhill direction flipped (sampling from below drags
/// the band upward — everything under the band lands ~0.8 of the band
/// value off), the two carry weights swapped (the far sample reading
/// stronger than the near one), and `max` degraded to overwrite (the
/// band's own values erased where the drag is weaker).
#[test]
fn sag_program_matches_the_cpu_oracle() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    // A band peaking at row 16, fading over 12 rows, swelling to the
    // right — values in [0, 0.9].
    let soft: Vec<f32> = (0..CANVAS_WIDTH * CANVAS_HEIGHT)
        .map(|i| {
            let (x, y) = ((i % CANVAS_WIDTH) as f32, (i / CANVAS_WIDTH) as f32);
            (1.0 - (y - 16.0).abs() / 12.0).max(0.0) * (0.3 + 0.7 * x / 95.0) * 0.9
        })
        .collect();
    let step_texels = 5u32;

    let soft_id = create_plane(&mut harness, "create_soft", &soft);
    let shown_id = create_shown_output(&mut harness);
    let program_id = register(
        &mut harness,
        &ProgramRegister {
            wgsl: format!("{PIGMENT_WGSL}\n{SHOW_WGSL}"),
            bindings: vec![plane_slot(), SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
            transients: vec![plane_slot()],
            passes: vec![
                sag_pass(InputSlot::Binding { index: 0 }, OutputSlot::Transient { index: 0 }, 0),
                show_pass(0, 1),
            ],
        },
    );

    let observed = developed_plane(
        &mut harness,
        program_id,
        vec![soft_id, shown_id],
        SagUniforms { step_texels }.encode(),
        shown_id,
    );
    assert_plane_close("sag", &sagged_oracle(&soft, step_texels as usize), &observed, TOLERANCE_REPLICATED);
}

/// Spatter parity: four pre-rolled drops stamped from the uniform blob
/// over a graded base, one bearing past pi (exercising the encoder's
/// wrap into WGSL's accurate `cos`/`sin` domain) and one disc crossing
/// the canvas edge. The plausible bugs this catches: a drop stamped at
/// the wrong bearing (the whole disc lands on the wrong side of the
/// centre — hundreds of steps off at both sites), the vertical droop
/// dropped (every landing too high), the stamp replacing the base
/// instead of adding to it, and the edge disc mis-clamped.
#[test]
fn spatter_program_matches_the_cpu_oracle() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    // A shallow vertical grade, so a stamp that replaces rather than
    // adds is visible everywhere a drop lands.
    let density: Vec<f32> =
        (0..CANVAS_WIDTH * CANVAS_HEIGHT).map(|i| 0.1 + 0.1 * (i / CANVAS_WIDTH) as f32 / 63.0).collect();
    let centre = Vec2::new(48.0, 24.0);
    let drops = [
        DropAccident { bearing: 0.6, throw: 18.0, radius: 4.0, strength: 0.6 },
        DropAccident { bearing: 2.3, throw: 26.0, radius: 3.0, strength: 0.5 },
        // Past pi, and thrown far enough up that its disc crosses the
        // canvas' top edge.
        DropAccident { bearing: 4.2, throw: 20.0, radius: 5.0, strength: 0.45 },
        DropAccident { bearing: 5.9, throw: 16.0, radius: 2.5, strength: 0.65 },
    ];

    let density_id = create_plane(&mut harness, "create_density", &density);
    let shown_id = create_shown_output(&mut harness);
    let program_id = register(
        &mut harness,
        &ProgramRegister {
            wgsl: format!("{PIGMENT_WGSL}\n{SHOW_WGSL}"),
            bindings: vec![plane_slot(), SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
            transients: vec![plane_slot()],
            passes: vec![
                spatter_pass(InputSlot::Binding { index: 0 }, OutputSlot::Transient { index: 0 }, 0),
                show_pass(0, 1),
            ],
        },
    );

    let observed = developed_plane(
        &mut harness,
        program_id,
        vec![density_id, shown_id],
        SpatterUniforms { centre, drops: &drops }.encode(),
        shown_id,
    );
    assert_plane_close("spatter", &spattered_oracle(&density, centre, &drops), &observed, TOLERANCE_TRIG);
}

/// Flow-smear parity against the real oracle — `image::smear_along_flow`
/// is public, so this comparison is against field code itself, not a
/// mirror: two advection passes over hand-built flow planes whose
/// bearings sweep the full circle and whose coherence straddles the
/// gate. The plausible bugs this catches: the advection reach off by the
/// half-texel slack (a bounds test on the unrounded position deflates
/// the tap count along every coherent edge — the fixture self-check
/// below proves such taps exist), the pass count wrong (one pass of
/// drag reads visibly shy of two), the flow axes swapped, and the
/// coherence gate or authority misapplied.
#[test]
fn smear_program_matches_the_cpu_oracle() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = boot();

    // A smooth field (low-octave noise), so a rounding-boundary tap swap
    // under driver fma stays within the stated tolerance.
    let density: Vec<f32> =
        Noise::new(0x5e_ed, 2, 4.0).plane(CANVAS_WIDTH, CANVAS_HEIGHT).iter().map(|&at| at * 0.9).collect();
    // Bearings sweeping the circle; coherence pinned high along the left
    // edge (where the slack taps live) and straddling the gate elsewhere.
    let mut flow = Flow { x: Vec::new(), y: Vec::new(), coherence: Vec::new() };
    for i in 0..CANVAS_WIDTH * CANVAS_HEIGHT {
        let (x, y) = (i % CANVAS_WIDTH, i / CANVAS_WIDTH);
        let bearing = x as f32 * 0.11 + y as f32 * 0.07;
        flow.x.push(bearing.cos());
        flow.y.push(bearing.sin());
        flow.coherence.push(if x < 8 {
            0.9
        } else {
            ((x * 7 + y * 13) % 32) as f32 / 32.0
        });
    }
    let reach = 4i32;

    // Fixture self-check: at least one coherent tap must round into the
    // plane from a position outside it (or the reverse), or the slack
    // path is never exercised and the scenario cannot catch its loss.
    let slack_taps = (0..CANVAS_WIDTH * CANVAS_HEIGHT)
        .filter(|&i| flow.coherence[i] >= 0.25)
        .flat_map(|i| (-reach..=reach).map(move |step| (i, step)))
        .filter(|&(i, step)| {
            let x = (i % CANVAS_WIDTH) as f32 + flow.x[i] * step as f32;
            let y = (i / CANVAS_WIDTH) as f32 + flow.y[i] * step as f32;
            let rounded_in = x.round() >= 0.0
                && x.round() <= (CANVAS_WIDTH - 1) as f32
                && y.round() >= 0.0
                && y.round() <= (CANVAS_HEIGHT - 1) as f32;
            let unrounded_in =
                x >= 0.0 && x <= (CANVAS_WIDTH - 1) as f32 && y >= 0.0 && y <= (CANVAS_HEIGHT - 1) as f32;
            rounded_in != unrounded_in
        })
        .count();
    assert!(slack_taps > 0, "the fixture must exercise the half-texel coverage slack");

    let density_id = create_plane(&mut harness, "create_density", &density);
    let flow_x_id = create_plane(&mut harness, "create_flow_x", &flow.x);
    let flow_y_id = create_plane(&mut harness, "create_flow_y", &flow.y);
    let coherence_id = create_plane(&mut harness, "create_coherence", &flow.coherence);
    let shown_id = create_shown_output(&mut harness);

    let slots = SmearSlots {
        density: InputSlot::Binding { index: 0 },
        flow_x: InputSlot::Binding { index: 1 },
        flow_y: InputSlot::Binding { index: 2 },
        coherence: InputSlot::Binding { index: 3 },
    };
    let mut passes = smear_passes(&slots, 0, OutputSlot::Transient { index: 1 }, 0).to_vec();
    passes.push(show_pass(1, 4));
    let program_id = register(
        &mut harness,
        &ProgramRegister {
            wgsl: format!("{PIGMENT_WGSL}\n{SHOW_WGSL}"),
            bindings: vec![
                plane_slot(),
                plane_slot(),
                plane_slot(),
                plane_slot(),
                SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full },
            ],
            transients: vec![plane_slot(), plane_slot()],
            passes,
        },
    );

    let observed = developed_plane(
        &mut harness,
        program_id,
        vec![density_id, flow_x_id, flow_y_id, coherence_id, shown_id],
        SmearUniforms { reach }.encode(),
        shown_id,
    );
    let expected = smear_along_flow(&density, &flow, CANVAS_WIDTH, CANVAS_HEIGHT, SMEAR_PASSES, reach);
    assert_plane_close("smear", &expected, &observed, TOLERANCE_SMEAR);
}
