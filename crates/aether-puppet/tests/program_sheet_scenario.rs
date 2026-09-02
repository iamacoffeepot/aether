//! The sheet ops driven standalone through the mail surface (ADR-0170,
//! iamacoffeepot/aether#4368): register the authored WGSL from
//! `easel/program/sheet.rs`, upload CPU-computed input planes as
//! registry textures, dispatch, draw the output through the established
//! overlay readback, capture, and hold the pixels against the CPU
//! oracle run on identical inputs.
//!
//! Observation is side-by-side: the GPU result and the quantized CPU
//! oracle are drawn as adjacent 1:1 rects in the same captured frame,
//! so both ride the identical nearest-sampled overlay -> sRGB surface
//! -> capture path and any difference between the rects is the op's
//! own, not the readback's. The `R32Float` ops append a test-only viz
//! entry to the module (the overlay pass warn-drops `R32Float` batches)
//! that lifts the data plane into gray RGBA8 through the same one
//! quantizer the oracle's gray upload uses.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]
// A test binary is its own compilation unit, so the crate-level cast
// allows do not reach it. Plane synthesis casts texel indices to `f32`
// and quantizes `[0, 1]` values to `u8` the same bounded way the easel
// does.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// The oracle restates the CPU wash formulas verbatim; `mul_add` / `hypot`
// rewrites would change the float semantics the parity is measured
// against.
#![allow(clippy::suboptimal_flops, clippy::imprecise_flops)]

use core::f32::consts::{SQRT_2, TAU};
use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, rgba_at};
use aether_harness_substrate_capture::visual::{Image, decode_png};
use aether_kinds::QuadSpace;
use aether_math::{Rgba, Vec2};
use aether_puppet::easel::image;
use aether_puppet::easel::palette::{self, Coat};
use aether_puppet::easel::program::sheet::{
    CoatParams, LostEdgeParams, SHEET_PARAMS_BYTES, SHEET_WGSL, care_mix_pass, coat_absorb_pass,
    first_coat_absorb_pass, lost_edge_pass, paper_composite_pass, plane_slot, sheet_slot,
};
use aether_render::QuadBlend;
use aether_render::{
    CreateTexture, CreateTextureResult, DrawTexturedQuads, InputSlot, OutputSlot, PassStage, ProgramDispatch,
    ProgramPass, ProgramRegister, ProgramRegisterResult, TextureFormat, TextureSampling, TextureUsage, TexturedQuad,
};

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

/// The test-only viz entry appended to the sheet module: lifts an
/// `R32Float` plane into gray RGBA8 so the overlay readback can observe
/// it. Reuses the module's own `plane_a` declaration.
const PLANE_VIEW_WGSL: &str = r"
@fragment
fn fs_plane_view(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let level = textureLoad(plane_a, vec2<i32>(position.xy), 0).r;
    return vec4<f32>(level, level, level, 1.0);
}
";

fn sheet_module() -> String {
    format!("{SHEET_WGSL}\n{PLANE_VIEW_WGSL}")
}

fn plane_view_pass(source: InputSlot, output: OutputSlot) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_plane_view".to_owned(),
        inputs: vec![source],
        output,
        uniform_offset: 0,
        uniform_length: 0,
        repeat: None,
    }
}

/// A `side` x `side` plane from a per-texel formula, row major.
fn plane(side: usize, value: impl Fn(usize, usize) -> f32) -> Vec<f32> {
    (0..side * side).map(|i| value(i % side, i / side)).collect()
}

/// A `[0, 1]` plane quantized to opaque gray RGBA8 — the oracle-side
/// twin of the `fs_plane_view` lift, one `round` against the GPU's one
/// unorm store.
fn gray_rgba(plane: &[f32]) -> Vec<u8> {
    plane
        .iter()
        .flat_map(|&value| {
            let level = (value * 255.0).round().clamp(0.0, 255.0) as u8;
            [level, level, level, u8::MAX]
        })
        .collect()
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

/// Upload one `f32` plane as an `R32Float` data-plane texture. Nearest
/// on principle (the values are quantities, ADR-0170); the passes read
/// with textureLoad regardless.
fn data_plane(harness: &mut SubstrateHarness, label: &'static str, side: usize, plane: &[f32]) -> u32 {
    create_texture(
        harness,
        label,
        &CreateTexture {
            width: side as u32,
            height: side as u32,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: plane.iter().flat_map(|value| value.to_le_bytes()).collect(),
        },
    )
}

/// An RGBA8 texture sampled nearest, so the 1:1 overlay draw reads each
/// texel exactly: the writable program output when `pixels` is empty,
/// the uploaded oracle rect otherwise.
fn rgba_nearest(harness: &mut SubstrateHarness, label: &'static str, side: usize, pixels: Vec<u8>) -> u32 {
    let usage = if pixels.is_empty() {
        TextureUsage::Writable
    } else {
        TextureUsage::Sampled
    };
    create_texture(
        harness,
        label,
        &CreateTexture {
            width: side as u32,
            height: side as u32,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage,
            pixels,
        },
    )
}

fn register_program(harness: &mut SubstrateHarness, mail: &ProgramRegister) -> u32 {
    let registered = harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("register sequence");
    match registered.reply::<ProgramRegisterResult>("register").expect("decode ProgramRegisterResult") {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    }
}

/// Row where both observation rects sit in every capture.
const RECT_TOP: u32 = 8;

/// A 1:1 overlay draw of `texture_id` at `(left, RECT_TOP)`: quad size
/// equals texture size, so each texel lands on exactly one framebuffer
/// pixel and nearest sampling reads it back unfiltered.
fn overlay(texture_id: u32, left: f32, side: usize) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        layer: 0,
        quads: vec![TexturedQuad {
            x: left,
            y: RECT_TOP as f32,
            width: side as f32,
            height: side as f32,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

/// Compare the GPU rect against the oracle rect texel by texel, RGB
/// channels, within `tolerance` captured 8-bit steps.
fn assert_rect_parity(img: &Image, gpu_left: u32, oracle_left: u32, side: u32, tolerance: u8, what: &str) {
    for row in 0..side {
        for column in 0..side {
            let gpu = rgba_at(img, gpu_left + column, RECT_TOP + row);
            let oracle = rgba_at(img, oracle_left + column, RECT_TOP + row);
            for channel in 0..3 {
                assert!(
                    gpu[channel].abs_diff(oracle[channel]) <= tolerance,
                    "{what}: texel ({column}, {row}) channel {channel}: gpu {gpu:?} vs oracle {oracle:?} \
                     (tolerance {tolerance})",
                );
            }
        }
    }
}

/// Dispatch the program and capture both observation rects in one frame.
fn capture_side_by_side(
    harness: &mut SubstrateHarness,
    dispatch: &ProgramDispatch,
    gpu_rect: &DrawTexturedQuads,
    oracle_rect: &DrawTexturedQuads,
) -> Image {
    let pre = vec![
        envelope("aether.render", dispatch),
        envelope("aether.render", gpu_rect),
        envelope("aether.render", oracle_rect),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture program output");
    decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png")
}

/// The care ramp applied (`fs_care_mix` against `field::material_wash`'s
/// mix): a horizontal tight gradient, a vertical loose gradient, and a
/// diagonal care ramp give every texel a distinct (held, freed, care)
/// triple, so the plausible port bugs — the weights swapped (tight
/// scaled by `1 - care`), or the three positional plane bindings
/// shuffled — each repaint whole gradients and blow far past tolerance.
///
/// Tolerance 2: the mix is exact linear `f32` arithmetic on both sides,
/// so the rects disagree only where the CPU `round` and the GPU unorm
/// store land a boundary value on different sides (one linear step),
/// and the shared sRGB encode's slope stays at or below one across the
/// `[0.25, 0.95]` band the gradients are built in.
#[test]
fn care_mix_matches_the_cpu_ramp() {
    if !require_wgpu_only() {
        return;
    }
    let side = 24;
    let span = (side - 1) as f32;
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let tight = plane(side, |x, _| 0.25 + 0.7 * x as f32 / span);
    let loose = plane(side, |_, y| 0.25 + 0.7 * y as f32 / span);
    let care = plane(side, |x, y| (x + y) as f32 / (2.0 * span));
    let mixed: Vec<f32> = tight
        .iter()
        .zip(&loose)
        .zip(&care)
        .map(|((&held, &freed), &care)| held * care + freed * (1.0 - care))
        .collect();

    let tight_id = data_plane(&mut harness, "tight", side, &tight);
    let loose_id = data_plane(&mut harness, "loose", side, &loose);
    let care_id = data_plane(&mut harness, "care", side, &care);
    let output_id = rgba_nearest(&mut harness, "output", side, Vec::new());
    let oracle_id = rgba_nearest(&mut harness, "oracle", side, gray_rgba(&mixed));

    let program_id = register_program(
        &mut harness,
        &ProgramRegister {
            wgsl: sheet_module(),
            bindings: vec![plane_slot(), plane_slot(), plane_slot(), sheet_slot()],
            transients: vec![plane_slot()],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![
                care_mix_pass(
                    InputSlot::Binding { index: 0 },
                    InputSlot::Binding { index: 1 },
                    InputSlot::Binding { index: 2 },
                    OutputSlot::Transient { index: 0 },
                ),
                plane_view_pass(InputSlot::PassOutput { pass: 0 }, OutputSlot::Binding { index: 3 }),
            ],
        },
    );

    let dispatch = ProgramDispatch {
        program_id,
        bindings: vec![tight_id, loose_id, care_id, output_id],
        geometries: Vec::new(),
        uniforms: Vec::new(),
    };
    let img =
        capture_side_by_side(&mut harness, &dispatch, &overlay(output_id, 4.0, side), &overlay(oracle_id, 36.0, side));

    assert_rect_parity(&img, 4, 36, side as u32, 2, "care mix");
}

/// The lost edge (`fs_lost_edge` against `field::threshold`'s lost
/// branch): radial hard and soft planes about a `(15.5, 15.5)` centroid
/// with the lost direction at 3.0 radians, so the giveback window spans
/// the atan2 seam at +-pi — half of it is reachable only through the
/// `min(away, TAU - away)` wrap, and dropping the wrap holds the edge
/// across that half. The other named bugs: the angular window flipped
/// (Hermite edges swapped puts the giveback on the held side), and the
/// missing `max(soft, 0)` clamp — a block of blur-residue texels a
/// rounding error below zero sits on the held side, where the CPU keeps
/// the hard edge but an unclamped `pow` turns NaN and the rect goes to
/// the unorm store's zero.
///
/// The oracle restates the CPU constants (`LOST_ARC` 1.3/0.55,
/// `LOST_FALLOFF` 1.8, `LOST_STAIN` 0.85) as its own literals, so a
/// drifted constant in the WGSL trips here rather than hiding behind a
/// shared definition.
///
/// Tolerance 4: outputs bottom out near the surviving stain (~0.10
/// linear, sRGB slope ~1.6), and the sides disagree by the two
/// quantizers plus atan2 / pow precision differences that move
/// `lostness` by ~2e-3 across the arc's ramp — under two linear steps
/// combined.
#[test]
fn lost_edge_gives_way_where_the_cpu_does() {
    if !require_wgpu_only() {
        return;
    }
    let side = 32;
    let mut harness = SubstrateHarness::builder().size(80, 48).with_render().build().expect("boot");

    let radius = |x: usize, y: usize| ((x as f32 - 15.5).powi(2) + (y as f32 - 15.5).powi(2)).sqrt();
    let residue = |x: usize, y: usize| (26..30).contains(&x) && (14..18).contains(&y);
    let hard = plane(side, |x, y| 0.6 + 0.3 * (1.0 - (radius(x, y) / 16.0).min(1.0)));
    let soft = plane(side, |x, y| {
        if residue(x, y) {
            -0.002
        } else {
            0.35 + 0.5 * (1.0 - (radius(x, y) / 16.0).min(1.0))
        }
    });

    let alpha: Vec<f32> = hard
        .iter()
        .zip(&soft)
        .enumerate()
        .map(|(i, (&hard, &soft))| {
            let bearing = ((i / side) as f32 - 15.5).atan2((i % side) as f32 - 15.5);
            let away = (bearing - 3.0).abs();
            let lostness = image::smoothstep(1.3, 0.55, away.min(TAU - away));
            hard * (1.0 - lostness) + soft.max(0.0).powf(1.8) * 0.85 * lostness
        })
        .collect();

    let hard_id = data_plane(&mut harness, "hard", side, &hard);
    let soft_id = data_plane(&mut harness, "soft", side, &soft);
    let output_id = rgba_nearest(&mut harness, "output", side, Vec::new());
    let oracle_id = rgba_nearest(&mut harness, "oracle", side, gray_rgba(&alpha));

    let program_id = register_program(
        &mut harness,
        &ProgramRegister {
            wgsl: sheet_module(),
            bindings: vec![plane_slot(), plane_slot(), sheet_slot()],
            transients: vec![plane_slot()],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![
                lost_edge_pass(
                    InputSlot::Binding { index: 0 },
                    InputSlot::Binding { index: 1 },
                    OutputSlot::Transient { index: 0 },
                    0,
                ),
                plane_view_pass(InputSlot::PassOutput { pass: 0 }, OutputSlot::Binding { index: 2 }),
            ],
        },
    );

    let dispatch = ProgramDispatch {
        program_id,
        bindings: vec![hard_id, soft_id, output_id],
        geometries: Vec::new(),
        uniforms: LostEdgeParams { centre: Vec2::new(15.5, 15.5), angle: 3.0 }.encode().to_vec(),
    };
    let img =
        capture_side_by_side(&mut harness, &dispatch, &overlay(output_id, 4.0, side), &overlay(oracle_id, 44.0, side));

    assert_rect_parity(&img, 4, 44, side as u32, 4, "lost edge");
}

/// The palette composite (`fs_first_coat_absorb` / `fs_coat_absorb` /
/// `fs_paper_composite` against [`palette::composite`] itself — the
/// real function is the oracle): three coats with distinct pigments and
/// caps over a non-unit paper shade, two densities deliberately
/// exceeding their caps. The named bugs: the cap applied after the
/// pigment power instead of before (the over-cap gradients then
/// over-darken whole bands), the per-coat uniform windows mis-strided
/// (every pigment/cap pairing differs, so a swap recolors a rect), the
/// paper white or the shade folded into every coat instead of once at
/// the resolve (the shade gradient would cube), and a sheet that is not
/// alpha-255 opaque paper — the convention the easel billboard depends
/// on, observed here because a non-opaque GPU rect blends toward the
/// near-black clear color while the oracle upload is opaque by
/// construction.
///
/// This parity is the tightest the surface allows for an op whose CPU
/// twin outputs RGBA8 directly. Tolerance 6: the GPU light accumulator
/// ping-pongs through RGBA8 between coats where the CPU carries `f32`,
/// so up to three quantization steps accumulate before the resolve's
/// own rounding, and the shared sRGB encode amplifies them by up to
/// ~1.6x at the darkest composite value the inputs reach (~0.11
/// linear).
#[test]
fn palette_composite_matches_the_cpu_sheet() {
    if !require_wgpu_only() {
        return;
    }
    let side = 24;
    let span = (side - 1) as f32;
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let reach = |x: usize, y: usize| ((x as f32 - 11.5).powi(2) + (y as f32 - 11.5).powi(2)).sqrt() / (11.5 * SQRT_2);
    let coats = [
        Coat {
            class: 1,
            pigment: 0x96_a0_c8,
            cap: palette::DENSITY_CAP,
            density: plane(side, |x, _| 1.6 * x as f32 / span),
        },
        Coat { class: 2, pigment: 0x8d_84_b8, cap: 0.8, density: plane(side, |_, y| 1.2 * y as f32 / span) },
        Coat {
            class: 3,
            pigment: 0x4a_56_61,
            cap: palette::DENSITY_CAP,
            density: plane(side, |x, y| 0.8 * reach(x, y)),
        },
    ];
    let shade = plane(side, |x, _| 0.96 + 0.08 * x as f32 / span);
    let oracle = palette::composite(&coats, &shade);

    let skin_id = data_plane(&mut harness, "skin_density", side, &coats[0].density);
    let glaze_id = data_plane(&mut harness, "glaze_density", side, &coats[1].density);
    let dress_id = data_plane(&mut harness, "dress_density", side, &coats[2].density);
    let shade_id = data_plane(&mut harness, "paper_shade", side, &shade);
    let sheet_id = rgba_nearest(&mut harness, "sheet", side, Vec::new());
    let oracle_id = rgba_nearest(&mut harness, "oracle", side, oracle);

    let program_id = register_program(
        &mut harness,
        &ProgramRegister {
            wgsl: sheet_module(),
            bindings: vec![plane_slot(), plane_slot(), plane_slot(), plane_slot(), sheet_slot()],
            transients: vec![sheet_slot(), sheet_slot()],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![
                first_coat_absorb_pass(InputSlot::Binding { index: 0 }, OutputSlot::Transient { index: 0 }, 0),
                coat_absorb_pass(
                    InputSlot::PassOutput { pass: 0 },
                    InputSlot::Binding { index: 1 },
                    OutputSlot::Transient { index: 1 },
                    SHEET_PARAMS_BYTES,
                ),
                coat_absorb_pass(
                    InputSlot::PassOutput { pass: 1 },
                    InputSlot::Binding { index: 2 },
                    OutputSlot::Transient { index: 0 },
                    2 * SHEET_PARAMS_BYTES,
                ),
                paper_composite_pass(
                    InputSlot::PassOutput { pass: 2 },
                    InputSlot::Binding { index: 3 },
                    OutputSlot::Binding { index: 4 },
                ),
            ],
        },
    );

    let dispatch = ProgramDispatch {
        program_id,
        bindings: vec![skin_id, glaze_id, dress_id, shade_id, sheet_id],
        geometries: Vec::new(),
        uniforms: coats.iter().flat_map(|coat| CoatParams { pigment: coat.pigment, cap: coat.cap }.encode()).collect(),
    };
    let img =
        capture_side_by_side(&mut harness, &dispatch, &overlay(sheet_id, 4.0, side), &overlay(oracle_id, 36.0, side));

    assert_rect_parity(&img, 4, 36, side as u32, 6, "palette composite");
}
