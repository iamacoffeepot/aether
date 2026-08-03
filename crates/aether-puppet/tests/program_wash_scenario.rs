//! Whole-sheet parity for the wash program (iamacoffeepot/aether#4369,
//! ADR-0170): the coat sequencer's one registered graph developed over a
//! synthetic subject, against `field::Sheet::coats` +
//! `palette::composite` on the identical planes and seed. This scenario
//! is the drift tripwire for everything downstream of the GPU wash: any
//! op, window, or sequencing change that moves the develop away from the
//! CPU oracle trips here first.
//!
//! The comparison space is the established one (see
//! `program_puddle_scenario.rs`): both sheets are drawn as adjacent 1:1
//! rects in one captured frame, the captured bytes are decoded back
//! through the inverse sRGB transfer, and differences are measured in
//! linear 8-bit steps. The budget, stated honestly: the GPU light
//! accumulator ping-pongs through `Rgba8` between coats where the CPU
//! carries `f32` — up to half a linear step per coat, ten coats deep —
//! and the shared sRGB encode quantizes at roughly two linear steps per
//! captured byte at paper brightness, so even identical math can sit a
//! few steps apart. On top of that ride two genuine discontinuities:
//! the atmosphere spill's hard cut and the threshold band over
//! iterated-tap blurs whose last bits differ from the CPU running sum —
//! either can flip an isolated texel and move a bounded neighbourhood by
//! tens of steps. Hence three thresholds instead of one tight max: the
//! mean must stay within [`MEAN_BUDGET`] steps (wrong math shifts whole
//! regions and explodes it), at most [`OUTLIER_FRACTION`] of texels may
//! exceed [`TEXEL_BUDGET`] steps (quantization noise stays under it;
//! only discontinuity flips go over), and nothing may exceed
//! [`WORST_BUDGET`] steps (a flipped texel's neighbourhood stays
//! bounded; a wrong constant does not). Failures name the worst texel,
//! the material under it, and the worst-diverging block.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]
// Plane synthesis casts texel indices to `f32` and back the same bounded
// way the easel crate itself does (see its crate-level rationale).
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// The sRGB inversion and the fixture geometry restate reference formulas
// verbatim; `mul_add` rewrites would change the float semantics the
// comparison space is defined in.
#![allow(clippy::suboptimal_flops, clippy::imprecise_flops)]

use std::env;
use std::time::Instant;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, rgba_at};
use aether_harness_substrate_capture::visual::{Image, decode_png};
use aether_kinds::QuadSpace;
use aether_math::{Mat4, Rgba, Vec2, Vec3};
use aether_puppet::chart::EyeFrame;
use aether_puppet::easel::accent;
use aether_puppet::easel::field::{Planes, Sheet};
use aether_puppet::easel::image::{self, Flow};
use aether_puppet::easel::palette;
use aether_puppet::easel::program::wash::{self, WashBindings};
use aether_puppet::labels::{BROW, DRESS, EYE, HAIR, INNER_EAR, LIPS, SKIN, TUFT};
use aether_render::{
    CreateTexture, CreateTextureResult, DrawTexturedQuads, ProgramDispatch, ProgramRegisterResult, TextureFormat,
    TextureSampling, TextureUsage, TexturedQuad,
};

/// The canvas the parity develops at: small enough that five hundred
/// tiny passes and the CPU oracle both run in seconds, large enough that
/// every coat has room to land — blur windows, wandered pours, an
/// atmosphere stain in the bare margin, two charted eyes.
const CANVAS_WIDTH: usize = 120;
const CANVAS_HEIGHT: usize = 160;

/// Mean absolute difference budget over every texel, in linear 8-bit
/// steps of the worst channel.
const MEAN_BUDGET: f32 = 1.5;

/// Per-texel budget ordinary quantization stays under, and the fraction
/// of texels allowed past it (the discontinuity flips).
const TEXEL_BUDGET: f32 = 8.0;
const OUTLIER_FRACTION: f32 = 0.02;

/// Absolute ceiling on any texel's divergence.
const WORST_BUDGET: f32 = 64.0;

/// Where the two observation rects sit in the captured frame.
const GPU_LEFT: u32 = 4;
const ORACLE_LEFT: u32 = 132;
const RECT_TOP: u32 = 8;

fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// The synthetic subject: a figure standing right of a bare-paper margin
/// so the atmosphere stain has air to land in, with every labelled
/// material present — skin with eyes, brows and remapped lips inside it,
/// an inner ear and a tuft at its edges, hair above, beside and below,
/// a dress at the bottom — a tone gradient across the whole sheet and a
/// facing plane that is frontal at the face and falls away below.
fn subject() -> (Vec<u8>, Vec<f32>, Vec<f32>) {
    let (width, height) = (CANVAS_WIDTH, CANVAS_HEIGHT);
    let mut classes = vec![0u8; width * height];
    let mut tone = vec![0.0f32; width * height];
    let mut facing = vec![0.0f32; width * height];

    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let over = |left: usize, right: usize, top: usize, bottom: usize| {
                (left..right).contains(&x) && (top..bottom).contains(&y)
            };

            classes[i] = if over(57, 76, 32, 49) || over(89, 108, 32, 49) {
                EYE
            } else if over(57, 76, 22, 28) || over(89, 108, 22, 28) {
                BROW
            } else if over(70, 93, 58, 66) {
                LIPS
            } else if over(44, 51, 18, 31) {
                INNER_EAR
            } else if over(112, 119, 14, 27) {
                TUFT
            } else if over(48, 115, 16, 72) {
                SKIN
            } else if over(36, 120, 0, 16) || over(36, 48, 0, 90) || over(40, 116, 72, 112) {
                HAIR
            } else if over(44, 112, 112, 156) {
                DRESS
            } else {
                0
            };

            let (across, down) = (x as f32 / width as f32, y as f32 / height as f32);
            tone[i] = (across + down) * 0.5;
            facing[i] = if down < 0.5 {
                1.0
            } else {
                (1.0 - 1.6 * (down - 0.5)).max(0.0)
            };
        }
    }

    (classes, tone, facing)
}

/// Where the fixture's eyes sit in world coordinates, and their iris
/// radius — sized so the projected eyes land inside the subject's EYE
/// patches at the proportions the accent policies were tuned for, and
/// small enough that the cheek apples hung off them stay on skin.
const EYE_WORLD_X: [f32; 2] = [0.05, 0.3167];
const EYE_WORLD_Y: f32 = 0.25;
const IRIS_RADIUS: f32 = 0.04;
const APERTURE: (f32, f32) = (0.07, 0.055);

/// A chart eye frame straight in front of the orthographic camera, the
/// same construction the accent unit tests plant.
fn eye_frame(world_x: f32) -> EyeFrame {
    let centre = Vec3::new(world_x, EYE_WORLD_Y, 0.0);
    let lid = |i: usize, rise: f32| {
        let across = -1.0 + 2.0 * i as f32 / 23.0;

        Vec3::new(world_x + APERTURE.0 * across, EYE_WORLD_Y + (1.0 - across * across) * APERTURE.1 * rise, 0.0)
    };

    EyeFrame {
        centre,
        width_tip: centre + Vec3::new(IRIS_RADIUS, 0.0, 0.0),
        height_tip: centre + Vec3::new(0.0, IRIS_RADIUS, 0.0),
        pupil: Vec2::new(0.26, 0.90),
        aperture: (0..24).map(|i| lid(i, 1.0)).chain((0..24).rev().map(|i| lid(i, -0.45))).collect(),
    }
}

/// Orthographic down -z over a unit square: world `(x, y)` maps to
/// canvas `((x + 0.5) * width, (0.5 - y) * height)`.
fn camera() -> Mat4 {
    Mat4::orthographic_rh(-0.5, 0.5, -0.5, 0.5, 1.0, 10.0)
        * Mat4::look_at_rh(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y)
}

/// Vertical stripes as stand-in ink: strong horizontal gradients, so the
/// structure-tensor flow runs vertically with near-full coherence and
/// the hair smear genuinely drags.
fn striped_ink() -> Vec<f32> {
    (0..CANVAS_WIDTH * CANVAS_HEIGHT).map(|i| f32::from(((i % CANVAS_WIDTH) / 3).is_multiple_of(2))).collect()
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

/// Upload one `f32` plane as an `R32Float` data texture.
fn data_plane(harness: &mut SubstrateHarness, label: &'static str, plane: &[f32]) -> u32 {
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

/// An `Rgba8` texture sampled nearest: the writable sheet when `pixels`
/// is empty, the uploaded oracle rect otherwise.
fn rgba_nearest(harness: &mut SubstrateHarness, label: &'static str, pixels: Vec<u8>) -> u32 {
    let usage = if pixels.is_empty() {
        TextureUsage::Writable
    } else {
        TextureUsage::Sampled
    };
    create_texture(
        harness,
        label,
        &CreateTexture {
            width: CANVAS_WIDTH as u32,
            height: CANVAS_HEIGHT as u32,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage,
            pixels,
        },
    )
}

/// A 1:1 overlay draw of one rect, texel per pixel.
fn overlay(texture_id: u32, left: u32) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![TexturedQuad {
            x: left as f32,
            y: RECT_TOP as f32,
            width: CANVAS_WIDTH as f32,
            height: CANVAS_HEIGHT as f32,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
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

/// What the painter's box calls the material under a texel.
fn material_name(class: u8) -> &'static str {
    match class {
        SKIN => "skin",
        DRESS => "dress",
        HAIR => "hair",
        INNER_EAR => "inner ear",
        TUFT => "tuft",
        LIPS => "lips",
        BROW => "brow",
        EYE => "eye",
        _ => "bare paper",
    }
}

/// Every coat the oracle lays for this subject, in `coats` order — the
/// non-vacuity contract the parity rides on: a coat that deposits
/// nothing is a coat the comparison cannot check.
const EXPECTED_COATS: [(&str, u8); 10] = [
    ("hair", HAIR),
    ("hair glaze", HAIR),
    ("hair atmosphere", HAIR),
    ("dress", DRESS),
    ("skin", SKIN),
    ("inner ear", INNER_EAR),
    ("ear tuft", TUFT),
    ("brow", BROW),
    ("iris", palette::IRIS),
    ("blush", SKIN),
];

#[test]
fn the_wash_program_develops_the_cpu_sheet() {
    if !require_wgpu_only() {
        return;
    }
    let began = Instant::now();
    let (width, height) = (CANVAS_WIDTH, CANVAS_HEIGHT);

    let (classes, tone, facing) = subject();
    let planes = Planes { classes: &classes, tone: &tone, facing: &facing, width, height };
    let accents = accent::paint(&EYE_WORLD_X.map(eye_frame), &camera(), &planes);
    let flow = image::structure_tensor_flow(&striped_ink(), width, height);
    assert_flow_reaches_the_hair(&flow, &classes);

    let sheet = Sheet::new(planes, 0x5e_ed);
    let coats = sheet.coats(Some(&flow), Some(&accents));
    assert_every_coat_contributes(&coats);
    let expected = palette::composite(&coats, sheet.paper_shade());

    let mut harness = SubstrateHarness::builder().size(256, 176).with_render().build().expect("boot");
    let iris = accents.mask(palette::IRIS).expect("the charted subject carries an iris mask");
    let bindings = WashBindings {
        classes: data_plane(&mut harness, "classes", &classes.iter().map(|&at| f32::from(at)).collect::<Vec<f32>>()),
        tone: data_plane(&mut harness, "tone", &tone),
        care: data_plane(&mut harness, "care", sheet.care()),
        tooth: data_plane(&mut harness, "tooth", &sheet.noise().tooth),
        edge: data_plane(&mut harness, "edge", &sheet.noise().edge),
        paper_shade: data_plane(&mut harness, "paper_shade", sheet.paper_shade()),
        flow_x: data_plane(&mut harness, "flow_x", &flow.x),
        flow_y: data_plane(&mut harness, "flow_y", &flow.y),
        coherence: data_plane(&mut harness, "coherence", &flow.coherence),
        lift: data_plane(&mut harness, "lift", &accents.lift),
        iris: data_plane(&mut harness, "iris", iris),
        blush: data_plane(&mut harness, "blush", &accents.blush),
        sheet: rgba_nearest(&mut harness, "sheet", Vec::new()),
    };
    let oracle_id = rgba_nearest(&mut harness, "oracle", expected);

    let program = wash::program();
    let registered = harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", program.register()))])
        .expect("register sequence");
    let program_id = match registered.reply::<ProgramRegisterResult>("register").expect("decode ProgramRegisterResult")
    {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    };

    let dispatch = ProgramDispatch {
        program_id,
        bindings: bindings.to_vec(),
        uniforms: program.uniforms(&sheet, Some(&flow), Some(&accents)),
    };
    let pre = vec![
        envelope("aether.render", &dispatch),
        envelope("aether.render", &overlay(bindings.sheet, GPU_LEFT)),
        envelope("aether.render", &overlay(oracle_id, ORACLE_LEFT)),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture developed sheet");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

    assert_sheet_parity(&img, &classes);
    eprintln!(
        "wash parity: {} passes, {} transients, {} uniform bytes, scenario {:.1?}",
        program.register().passes.len(),
        program.register().transients.len(),
        dispatch.uniforms.len(),
        began.elapsed(),
    );
}

/// The smear must have something to ride: without coherent flow inside
/// the hair the two advection passes are identity on both sides and the
/// parity would hold vacuously over the one op the hair alone exercises.
fn assert_flow_reaches_the_hair(flow: &Flow, classes: &[u8]) {
    let coherent = flow
        .coherence
        .iter()
        .zip(classes)
        .filter(|&(_, &class)| class == HAIR)
        .map(|(&at, _)| at)
        .fold(0.0f32, f32::max);
    assert!(coherent > 0.5, "the striped ink must give the hair coherent flow, got at most {coherent}");
}

/// Every expected coat present and depositing somewhere: 0.01 sits
/// comfortably above the composite's own minimum-deposit skip, so a
/// passing coat is one the composite genuinely darkens the sheet with.
fn assert_every_coat_contributes(coats: &[palette::Coat]) {
    assert_eq!(coats.len(), EXPECTED_COATS.len(), "the subject must develop every coat in the box");
    for ((name, class), coat) in EXPECTED_COATS.iter().zip(coats) {
        assert_eq!(coat.class, *class, "coat order drifted at {name}");
        assert!(
            coat.density.iter().any(|&at| at.min(coat.cap) > 0.01),
            "the {name} coat deposits nothing anywhere — the parity over it would be vacuous",
        );
    }
}

/// Compare the two rects in inverse-sRGB linear space, reporting the
/// worst texel (with the material under it) and the worst 20x20 block.
fn assert_sheet_parity(img: &Image, classes: &[u8]) {
    const BLOCK: usize = 20;
    let (width, height) = (CANVAS_WIDTH, CANVAS_HEIGHT);
    let blocks_across = width / BLOCK;
    let mut block_sums = vec![0.0f32; blocks_across * (height / BLOCK)];
    let (mut sum, mut over_budget, mut worst, mut worst_at) = (0.0f64, 0usize, 0.0f32, 0usize);

    for y in 0..height {
        for x in 0..width {
            let gpu = rgba_at(img, GPU_LEFT + x as u32, RECT_TOP + y as u32);
            let oracle = rgba_at(img, ORACLE_LEFT + x as u32, RECT_TOP + y as u32);
            let steps = (0..3)
                .map(|channel| (srgb_to_linear(gpu[channel]) - srgb_to_linear(oracle[channel])).abs() * 255.0)
                .fold(0.0f32, f32::max);

            sum += f64::from(steps);
            if steps > TEXEL_BUDGET {
                over_budget += 1;
            }
            if steps > worst {
                (worst, worst_at) = (steps, y * width + x);
            }
            block_sums[y / BLOCK * blocks_across + x / BLOCK] += steps;
        }
    }

    let mean = (sum / (width * height) as f64) as f32;
    let outliers = over_budget as f32 / (width * height) as f32;
    let (worst_block, worst_block_sum) =
        block_sums.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).expect("the canvas has blocks");
    let report = format!(
        "worst texel ({}, {}) over {} drifts {worst:.1} steps; worst block ({}, {}) means {:.2} steps; \
         global mean {mean:.3} steps, {:.2}% of texels past {TEXEL_BUDGET}",
        worst_at % width,
        worst_at / width,
        material_name(classes[worst_at]),
        worst_block % blocks_across * BLOCK,
        worst_block / blocks_across * BLOCK,
        worst_block_sum / (BLOCK * BLOCK) as f32,
        outliers * 100.0,
    );

    assert!(mean <= MEAN_BUDGET, "the GPU develop drifts from the CPU sheet: {report}");
    assert!(outliers <= OUTLIER_FRACTION, "too many texels past the per-texel budget: {report}");
    assert!(worst <= WORST_BUDGET, "a texel diverges past any quantization account: {report}");
}
