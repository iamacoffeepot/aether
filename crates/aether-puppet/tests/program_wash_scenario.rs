//! Whole-sheet parity for the wash program (iamacoffeepot/aether#4369,
//! #4387, ADR-0170/0171): the coat sequencer's one registered graph
//! developed over a synthetic subject, against `field::Sheet::coats` +
//! `palette::composite` on the identical planes and seed. This scenario is
//! the drift tripwire for everything downstream of the GPU wash: any op,
//! window, or sequencing change that moves the develop away from the CPU
//! oracle trips here first.
//!
//! # Two develops, one oracle
//!
//! The shipped graph develops its body at [`wash::BODY_DIVISOR`] times
//! coarser than the sheet (the `SlotExtent::Divided` notch), so it cannot
//! be held against a full-resolution CPU oracle at a budget that means
//! anything about the *math*: half the difference would be the notch. So
//! the scenario lays the graph twice through `wash::program_at`.
//!
//! **Un-notched** (`divisor` 1) is the correctness gate. Every slot
//! resolves to the sheet's own extent, so the only thing between the two
//! develops is arithmetic, and it carries the tight budget the wash has
//! always carried.
//!
//! **Notched** (the shipped divisor) is the *measurement*. It runs the
//! identical inputs and reports what developing coarsely costs, against
//! budgets stated separately and honestly — they are wider, and they are
//! wider because a half-rate wash body is a different picture, not because
//! anything is wrong with it.
//!
//! The comparison space is the established one (see
//! `program_puddle_scenario.rs`): sheets are drawn as adjacent 1:1 rects
//! in one captured frame, the captured bytes are decoded back through the
//! inverse sRGB transfer, and differences are measured in linear 8-bit
//! steps. The budget for the un-notched pass, stated honestly: the GPU
//! light accumulator ping-pongs through `Rgba8` between coats where the
//! CPU carries `f32` — up to half a linear step per coat, ten coats deep —
//! and the shared sRGB encode quantizes at roughly two linear steps per
//! captured byte at paper brightness, so even identical math can sit a few
//! steps apart. On top of that ride the genuine discontinuities: the
//! atmosphere spill's hard cut, the threshold band over iterated-tap blurs
//! whose last bits differ from the CPU running sum, the care field's jump
//! flood against the CPU's chamfer sweeps, and the aperture clip's
//! rasterized fan against the CPU's scanline fill — any of which can flip
//! an isolated texel and move a bounded neighbourhood by tens of steps.
//! Hence three thresholds instead of one tight max.
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
use aether_math::{Mat4, Rgb, Rgba, Vec2, Vec3};
use aether_puppet::chart::EyeFrame;
use aether_puppet::easel::field::{self, Planes, Sheet};
use aether_puppet::easel::image::{self, Flow};
use aether_puppet::easel::program::wash::{self, Canvas, Faces, Frame, Placement, Presence, WashBindings, WashProgram};
use aether_puppet::easel::program::{face, ink};
use aether_puppet::easel::survey::SLOTS;
use aether_puppet::easel::{accent, palette};
use aether_puppet::labels::{BROW, DRESS, EYE, HAIR, INNER_EAR, LIPS, SKIN, TUFT};
use aether_render::QuadBlend;
use aether_render::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DrawTexturedQuads, DrawTriangle,
    ProgramDispatch, ProgramRegisterResult, TextureFormat, TextureSampling, TextureUsage, TexturedQuad, Vertex,
};

/// The canvas the parity develops at: small enough that five hundred
/// tiny passes and the CPU oracle both run in seconds, large enough that
/// every coat has room to land — blur windows, wandered pours, an
/// atmosphere stain in the bare margin, two charted eyes.
const CANVAS_WIDTH: usize = 120;
const CANVAS_HEIGHT: usize = 160;

const SEED: u64 = 0x5e_ed;

/// Budgets for the un-notched develop, in linear 8-bit steps of the worst
/// channel: the mean over every texel, the per-texel ceiling ordinary
/// quantization stays under with the fraction of texels allowed past it
/// (the discontinuity flips), and the absolute ceiling.
///
/// The ceiling is a whole step rather than a fraction of one, and the
/// reason is the fixture's own scale rather than anything in the develop.
/// This canvas' irises project to a radius of six texels; the iris rim
/// ramps over `IRIS_RIM`'s 0.15 of a radius, which is *one texel* here.
/// So where the rasterized aperture fan and the CPU's scanline fill
/// disagree about a boundary texel — which they do, by construction, and
/// over 1.4% of the iris' total coverage as measured against the CPU
/// accents directly — the iris' own heavy pigment turns that one texel
/// into a full step. At the framing the accents are actually tuned for an
/// iris is a couple of dozen texels across and the same ramp spans three
/// or four, so this is the fixture paying for being small enough to run
/// the CPU oracle in a test. What the ceiling still catches is a
/// *neighbourhood* going wrong, because `outliers` holds the count down.
const EXACT: Budget = Budget { mean: 1.5, texel: 8.0, outliers: 0.02, worst: 224.0 };

/// Budgets for the notched develop against the same full-resolution
/// oracle. A wash body developed at half rate is a different picture:
/// every tide line is decided on a grid twice as coarse and lifted back
/// bilinearly, so whole regions sit a step or two off and the edges
/// themselves move by up to a texel.
///
/// These are wide, and wider than the notch costs in production, because
/// the fixture is small. At 120x160 the body develops at 60x80, where the
/// loose wash's own water radius — twelve reference pixels — resolves to
/// *under one texel*: the instruments quantize before the picture does.
/// At the framing the engine is tuned for the same body is 450x600 and
/// that radius is six texels, so the notch costs the develop rather less
/// there. What the pinned-framing picture measures is the number that
/// decides whether the notch is worth taking; what these numbers are for
/// is a regression — a notch that has started swallowing the accents, or
/// a seam that has stopped lifting — and neither is subtle enough to hide
/// under them.
const NOTCHED: Budget = Budget { mean: 12.0, texel: 24.0, outliers: 0.16, worst: 255.0 };

struct Budget {
    mean: f32,
    texel: f32,
    outliers: f32,
    worst: f32,
}

/// Where the three observation rects sit in the captured frame.
const EXACT_LEFT: u32 = 4;
const NOTCHED_LEFT: u32 = 132;
const ORACLE_LEFT: u32 = 260;
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

/// How wide one stripe of the stand-in ink runs, and its period, in canvas
/// pixels.
const STRIPE: usize = 3;

/// Vertical stripes as stand-in ink: strong horizontal gradients, so the
/// structure-tensor flow runs vertically with near-full coherence and the
/// hair smear genuinely drags.
fn striped_ink() -> Vec<f32> {
    (0..CANVAS_WIDTH * CANVAS_HEIGHT).map(|i| f32::from(((i % CANVAS_WIDTH) / STRIPE).is_multiple_of(2))).collect()
}

/// The same stripes as ribbon triangles for the ink pass to rasterize.
///
/// The flow field is solved on the GPU now, off whatever the ink pass
/// draws, so the two develops only share a flow if they share a drawing.
/// Each stripe is a quad spanning its own three pixel columns; the camera
/// maps world x linearly onto the canvas, so a quad's edges land exactly
/// between pixel centres and the rasterized coverage is the CPU plane
/// texel for texel.
fn striped_ribbons() -> Vec<DrawTriangle> {
    let at = |x: usize, y: f32| Vertex {
        x: x as f32 / CANVAS_WIDTH as f32 - 0.5,
        y,
        z: 0.0,
        color: Rgb::new(0.0, 0.0, 0.0),
    };

    (0..CANVAS_WIDTH.div_ceil(2 * STRIPE))
        .flat_map(|band| {
            let (left, right) = (band * 2 * STRIPE, (band * 2 * STRIPE + STRIPE).min(CANVAS_WIDTH));
            [
                DrawTriangle { verts: [at(left, -0.5), at(right, -0.5), at(right, 0.5)] },
                DrawTriangle { verts: [at(left, -0.5), at(right, 0.5), at(left, 0.5)] },
            ]
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

fn create_geometry(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateGeometry) -> u32 {
    let created = harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("create_geometry sequence");
    match created.reply::<CreateGeometryResult>(label).expect("decode CreateGeometryResult") {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { reason } => panic!("create_geometry ({label}) failed: {reason}"),
    }
}

/// One `f32` plane as an `R32Float` data texture at the extent it was
/// pulped at.
fn data_plane(harness: &mut SubstrateHarness, label: &'static str, size: (usize, usize), plane: &[f32]) -> u32 {
    create_texture(
        harness,
        label,
        &CreateTexture {
            width: size.0 as u32,
            height: size.1 as u32,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: plane.iter().flat_map(|value| value.to_le_bytes()).collect(),
        },
    )
}

/// The bake's packed plane, staged from the CPU planes rather than drawn.
///
/// This scenario is the wash's parity, not the bake's — `program_bake_scenario`
/// holds the rasterized plane against these same inputs — so the packed
/// texture is written here directly, in the channel order and encoding
/// `bake.wgsl`'s header fixes: class as `class / 255`, tone and facing as
/// plain unorms. Point-sampled down to the body's extent for the notched
/// develop, which is what a bake at that extent resolves each texel to.
fn packed_plane(
    harness: &mut SubstrateHarness,
    label: &'static str,
    size: (usize, usize),
    classes: &[u8],
    tone: &[f32],
    facing: &[f32],
) -> u32 {
    let (width, height) = size;
    let step = CANVAS_WIDTH / width;
    let mut pixels = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let source = (y * step).min(CANVAS_HEIGHT - 1) * CANVAS_WIDTH + (x * step).min(CANVAS_WIDTH - 1);
            let unorm = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
            pixels.extend_from_slice(&[classes[source], unorm(tone[source]), unorm(facing[source]), 255]);
        }
    }

    create_texture(
        harness,
        label,
        &CreateTexture {
            width: width as u32,
            height: height as u32,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels,
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
        blend: QuadBlend::Straight,
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

/// One develop's registered program, its bindings and its dispatch —
/// everything the harness needs to run one graph at one divisor.
struct Develop {
    program: WashProgram,
    bindings: WashBindings,
    geometries: Vec<u32>,
}

#[test]
fn the_wash_program_develops_the_cpu_sheet() {
    if !require_wgpu_only() {
        return;
    }
    let began = Instant::now();
    let (width, height) = (CANVAS_WIDTH, CANVAS_HEIGHT);
    let canvas = Canvas { width, height };

    let (classes, tone, facing) = subject();
    let planes = Planes { classes: &classes, tone: &tone, facing: &facing, width, height };
    let frames = EYE_WORLD_X.map(eye_frame);
    let accents = accent::paint(&frames, &camera(), &planes);
    let flow = image::structure_tensor_flow(&striped_ink(), width, height);
    assert_flow_reaches_the_hair(&flow, &classes);

    let sheet = Sheet::new(planes, SEED);
    let coats = sheet.coats(Some(&flow), Some(&accents));
    assert_every_coat_contributes(&coats);
    let expected = palette::composite(&coats, sheet.paper_shade());

    let mut harness = SubstrateHarness::builder().size(384, 176).with_render().build().expect("boot");
    let oracle_id = rgba_nearest(&mut harness, "oracle", expected);

    // Where each wash places its accidents. Taken off the same pixels the
    // oracle takes them off, so what this scenario holds is the program's
    // own arithmetic; the geometry-derived centroids `survey` answers with
    // in production are gated by the pinned-framing picture instead, which
    // is the only place there is a subject to survey.
    let mut centroids: [Option<Vec2>; SLOTS] = [None; SLOTS];
    for material in palette::MATERIALS.iter().filter(|material| material.class < palette::META) {
        centroids[usize::from(material.class)] = field::centroid(&palette::mask_of(&classes, material.class), width);
    }
    // The stain's pole, taken off the oracle's own spill pixels. The
    // develop estimates this off the geometry (`easel::stain_centres`),
    // and that estimate is a picture-level judgement gated by the pinned
    // framing — feeding it in here instead would put it underneath every
    // other comparison in the scenario.
    let mut stains: [Option<Vec2>; SLOTS] = [None; SLOTS];
    for material in palette::MATERIALS {
        if let Some(policy) = material.atmosphere.as_ref() {
            let mask = palette::mask_of(&classes, material.class);
            stains[usize::from(material.class)] = field::centroid(&sheet.atmosphere_spill(&mask, policy), width);
        }
    }
    let iris = accents.mask(palette::IRIS).and_then(|mask| field::centroid(mask, width));

    let fine_eyes = accent::project(&frames, &camera(), width, height);
    let presence = accent::presences(&fine_eyes, &planes);

    let mut reports = Vec::new();
    for (divisor, left, budget, label) in
        [(1u32, EXACT_LEFT, &EXACT, "un-notched"), (wash::BODY_DIVISOR, NOTCHED_LEFT, &NOTCHED, "notched")]
    {
        let develop = stage(&mut harness, divisor, canvas, &classes, &tone, &facing, &fine_eyes);
        let body = canvas.body_at(divisor);
        let body_eyes = accent::project(&frames, &camera(), body.0, body.1);
        // Every body-extent chain places its accidents in the body's own
        // texels, so the centroids taken off the full-resolution oracle
        // scale down with it. A centroid is a mean, and a uniform
        // downsample scales a mean exactly. The iris does not: its chain
        // develops at the sheet's own pixels either way.
        let scale = 1.0 / divisor as f32;
        let notched = |at: &[Option<Vec2>; SLOTS]| at.map(|centre| centre.map(|at| at * scale));
        let (centroids, stains) = (notched(&centroids), notched(&stains));
        let placement = Placement { centroids: &centroids, stains: &stains, iris };
        let frame = Frame {
            view_proj: camera(),
            placement: Placement { centroids: &centroids, stains: &stains, iris },
            faces: Some(Faces { fine: &fine_eyes, body: &body_eyes, presence: &presence }),
        };
        let seed = develop.program.seed_uniforms(SEED, canvas, Presence::of(&placement));

        let dispatch = ProgramDispatch {
            program_id: register(&mut harness, &develop.program),
            bindings: develop.bindings.to_vec(),
            geometries: develop.geometries.clone(),
            uniforms: develop.program.frame_uniforms(&seed, &frame),
        };
        let pre = vec![
            envelope("aether.render", &dispatch),
            envelope("aether.render", &overlay(develop.bindings.sheet, left)),
            envelope("aether.render", &overlay(oracle_id, ORACLE_LEFT)),
        ];
        let captured = harness
            .execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))])
            .expect("capture developed sheet");
        let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png");

        reports.push(format!(
            "{label} (divisor {divisor}): {} passes, {} transients, {} uniform bytes — {}",
            develop.program.register().passes.len(),
            develop.program.register().transients.len(),
            dispatch.uniforms.len(),
            assert_sheet_parity(&img, left, &classes, budget, label),
        ));
    }

    eprintln!("wash parity, scenario {:.1?}:\n  {}", began.elapsed(), reports.join("\n  "));
}

/// Register one graph and hand back its id.
fn register(harness: &mut SubstrateHarness, program: &WashProgram) -> u32 {
    let registered = harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", program.register()))])
        .expect("register sequence");
    match registered.reply::<ProgramRegisterResult>("register").expect("decode ProgramRegisterResult") {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    }
}

/// Everything one develop binds: the bake's packed plane and the paper,
/// each at the extent its own reader develops at, plus the two geometries
/// the drawing and the chart supply.
fn stage(
    harness: &mut SubstrateHarness,
    divisor: u32,
    canvas: Canvas,
    classes: &[u8],
    tone: &[f32],
    facing: &[f32],
    eyes: &[accent::Eye],
) -> Develop {
    let body = canvas.body_at(divisor);
    let fine = (canvas.width, canvas.height);
    let coarse = field::paper(SEED, body.0, body.1);
    let sharp = field::paper(SEED, fine.0, fine.1);

    let bindings = WashBindings {
        packed: packed_plane(harness, "packed", body, classes, tone, facing),
        tooth: data_plane(harness, "tooth", body, &coarse.noise.tooth),
        edge: data_plane(harness, "edge", body, &coarse.noise.edge),
        tooth_fine: data_plane(harness, "tooth_fine", fine, &sharp.noise.tooth),
        edge_fine: data_plane(harness, "edge_fine", fine, &sharp.noise.edge),
        paper_shade: data_plane(harness, "paper_shade", fine, &sharp.shade),
        sheet: rgba_nearest(harness, "sheet", Vec::new()),
    };

    let ribbons = striped_ribbons();
    let geometries = vec![
        create_geometry(
            harness,
            "ink_geometry",
            &CreateGeometry {
                layout: ink::geometry_slot().layout,
                vertices: ink::vertices(&ribbons),
                indices: ink::indices(&ribbons),
            },
        ),
        create_geometry(
            harness,
            "aperture_geometry",
            &CreateGeometry {
                layout: face::geometry_slot().layout,
                vertices: face::vertices(eyes, canvas.width, canvas.height),
                indices: face::indices(eyes, canvas.width, canvas.height),
            },
        ),
    ];

    Develop { program: wash::program_at(canvas.height, divisor), bindings, geometries }
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

/// Compare one develop's rect against the oracle in inverse-sRGB linear
/// space, reporting the worst texel (with the material under it) and the
/// worst 20x20 block. Returns the report so a passing run states what it
/// measured rather than only that it passed.
fn assert_sheet_parity(img: &Image, left: u32, classes: &[u8], budget: &Budget, label: &str) -> String {
    const BLOCK: usize = 20;
    let (width, height) = (CANVAS_WIDTH, CANVAS_HEIGHT);
    let blocks_across = width / BLOCK;
    let mut block_sums = vec![0.0f32; blocks_across * (height / BLOCK)];
    let (mut sum, mut over_budget, mut worst, mut worst_at) = (0.0f64, 0usize, 0.0f32, 0usize);

    for y in 0..height {
        for x in 0..width {
            let gpu = rgba_at(img, left + x as u32, RECT_TOP + y as u32);
            let oracle = rgba_at(img, ORACLE_LEFT + x as u32, RECT_TOP + y as u32);
            let steps = (0..3)
                .map(|channel| (srgb_to_linear(gpu[channel]) - srgb_to_linear(oracle[channel])).abs() * 255.0)
                .fold(0.0f32, f32::max);

            sum += f64::from(steps);
            if steps > budget.texel {
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
         global mean {mean:.3} steps, {:.2}% of texels past {}",
        worst_at % width,
        worst_at / width,
        material_name(classes[worst_at]),
        worst_block % blocks_across * BLOCK,
        worst_block / blocks_across * BLOCK,
        worst_block_sum / (BLOCK * BLOCK) as f32,
        outliers * 100.0,
        budget.texel,
    );

    assert!(mean <= budget.mean, "the {label} develop drifts from the CPU sheet: {report}");
    assert!(outliers <= budget.outliers, "too many {label} texels past the per-texel budget: {report}");
    assert!(worst <= budget.worst, "a {label} texel diverges past any quantization account: {report}");

    report
}
