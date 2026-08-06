//! GPU-versus-CPU parity for the stroke visibility field
//! (iamacoffeepot/aether#4418, ADR-0172): `easel/program/sight.rs`'s
//! prepass and scans driven through the `aether.render` mail surface
//! against `visibility::runs` on the identical mesh, drawing, camera
//! and surface bias.
//!
//! # What is being compared, and in what space
//!
//! The probe observes three derived `R32Float` planes and the reference
//! plane's sign, and nothing reads float pixels back to the CPU — the
//! only pixel exit in the engine is the frame capture. So the planes are
//! observed the way every parity scenario here observes one: a
//! test-only probe pass packs them into an `Rgba8` writable binding, the
//! overlay draws that texture
//! texel-for-pixel, and the capture's bytes are decoded back through
//! the inverse sRGB transfer. The field is the canvas's own extent, so
//! one texel is one pixel and the mapping is exact rather than
//! resampled.
//!
//! The instrument's own floor, stated: `seen` and the reference sign are
//! two booleans packed into red's exact `0, 85, 170, 255` byte codes; the
//! sRGB round trip returns the same code, so both verdicts are compared
//! *exactly*. `reach` rides as a fraction of [`REACH_WINDOW`] and
//! `coverage` as itself, both quantizing once at the store and once at
//! the encode — about one part in 255 of their encoded range each.
//!
//! # The two gates, and why they are separate
//!
//! **Parity** ([`assert_verdicts`]) holds the GPU's verdict against the
//! oracle's, per point. The oracle here is `visibility::runs` at stride
//! 1 — the exact walk, not the strided-and-refined one the shipped
//! frame uses. Stride 3 is an approximation of stride 1 by design
//! (`VISIBILITY_STRIDE`'s own note measures the two as differing by a
//! hundredth of a percent of the drawing's points), so gating against
//! it would fold that approximation's error into a measurement of this
//! one.
//!
//! **Derivation** ([`assert_derived`]) holds `reach` and `coverage`
//! against the same quantities computed on the CPU *from the GPU's own
//! verdict plane*. The two gates decompose deliberately: a scan bug and
//! a bias disagreement have nothing to do with each other, and running
//! the derived fields against a CPU verdict vector that differs at a
//! handful of silhouette texels would put that difference into every
//! number and hide the scan's own.
//!
//! # The tolerance that matters
//!
//! Not per-point verdict equality. The oracle casts a ray from a point
//! lifted along its normal and asks the BVH what it meets; the field
//! projects that same lifted point and compares against the nearest
//! surface at its *pixel's centre*. Where a stroke runs along a
//! silhouette the two are asking about the same surface at a grazing
//! angle, and a pixel's width of difference in where the surface is
//! flips the verdict. That is the one-texel-of-bias difference ADR-0172
//! anticipates, and it is bounded rather than waved through: every
//! disagreement must be a point whose verdict *the oracle itself* does
//! not hold across the texel's own neighbourhood. [`marginal`] asks it
//! directly — it walks the probe out through [`SAMPLE_PIXELS`] of the
//! view and casts again through `visibility::hidden` — so the tolerance
//! is a statement about the geometry rather than a tuned count.
//! [`SETTLED_BUDGET`] leaves room for the one difference no resample
//! can reach, the sub-texel feature, and [`VERDICT_BUDGET`] sits behind
//! both as a backstop against the invariant going vacuous.

// Integration-test skip diagnostic and instrument reporting: emit via
// stderr so `cargo test -- --nocapture` surfaces them alongside the
// test name (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle and the instrument's
// own AETHER_CROSSFEED_DIR — test-harness knobs, not cap config.
#![allow(clippy::disallowed_methods)]
// A test binary is its own compilation unit, so the crate-level allows
// do not reach it. Field indexing casts between texel indices and `f32`
// coordinates the same bounded way the easel does, and the oracle-side
// transcriptions must stay textually identical to the formulas they
// mirror, so no `mul_add` rewrites.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]

use core::f32::consts::{PI, TAU};
use core::fmt::Write as _;
use core::iter;
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
use aether_puppet::easel::program::sight::{self, Layout, SightUniforms, ToneUniforms};
use aether_puppet::extract::{self, Settings};
use aether_puppet::feature::{Curve3, Drawing, FeatureClass, Pen, SurfacePoint};
use aether_puppet::labels::Labels;
use aether_puppet::mesh::Mesh;
use aether_puppet::{Pose, anchor, chart, deform, ribbon, style, visibility};
use aether_render::QuadBlend;
use aether_render::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DrawTexturedQuads, InputSlot, OutputSlot,
    PassStage, ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, SlotExtent, SlotSpec,
    TextureFormat, TextureSampling, TextureUsage, TexturedQuad, UpdateGeometry,
};

/// The canvas the CI parity runs at. The field is the canvas's own
/// extent, so this is also the capacity the drawing must lay out
/// inside — 19200 texels against the fixture's few thousand.
const CANVAS_WIDTH: usize = 120;
const CANVAS_HEIGHT: usize = 160;

/// The framing the easel itself develops at (`Puppet::init`'s `Look`),
/// and the field of view it projects through.
const ELEVATION: f32 = 3.0;
const DISTANCE: f32 = 5.4;
const FIELD_OF_VIEW: f32 = 0.454;

/// The four eyes every gate here runs at. Zero is the framing the easel
/// develops at; the other three turn her far enough that the silhouette
/// is a different curve and different parts of her occlude each other,
/// so a bias tuned to one profile cannot carry the gate.
const AZIMUTHS: [f32; 4] = [0.0, 30.0, 55.0, 90.0];

/// Padding the canonical cross-feed material field was baked with.
const LABEL_PAD: f32 = 0.12;

/// Umbrella passes over the vertex normals, as `Settings::default`
/// asks for and the shipped component loads its subject with.
const RELAXATION: usize = 2;

/// Arc the probe encodes `reach` against, in radians. Three times
/// `style::pressure`'s `RAMP` of 0.0064, so the whole ramp the taper
/// reads lands inside the encoded range with room past it, and one
/// recovered step is `REACH_WINDOW / 255`.
const REACH_WINDOW: f32 = 0.02;

/// How many rings and spokes the resample walks, and how far out the
/// outermost ring sits, in pixels.
///
/// The two mechanisms ask the same question of the same surface and
/// differ only in *where* they sample it: the oracle casts a ray
/// through the probe itself, the field reads the depth image at the
/// centre of the texel the probe projects into. So a disagreement is a
/// tie-break exactly when the oracle's own answer is not stable across
/// that neighbourhood — which [`marginal`] asks it directly, by moving
/// the probe across the view and casting again through
/// `visibility::hidden`.
///
/// Measured rather than picked: on the shipped subject at the framing
/// the easel develops at, 344 of the 346 disagreeing points resolve at
/// the innermost half-pixel ring and one more at 2.5, so the window is
/// set at two pixels — past where the distribution has anything left in
/// it, and short of the radius at which a resample would start finding
/// unrelated geometry and calling every difference marginal. Eight
/// spokes because a four-direction cross misses a probe sitting on the
/// diagonal of an edge: at one pixel it left 38 of those 346
/// unexplained, and the ring leaves one.
const SAMPLE_PIXELS: f32 = 2.0;
const SAMPLE_RINGS: u32 = 4;
const SAMPLE_SPOKES: u32 = 8;

/// What fraction of the drawing may disagree in a way no resample
/// explains.
///
/// Not zero, and the reason is structural rather than a tolerance for
/// error. The depth image holds, per texel, the nearest surface the
/// rasterizer covered anywhere in it; the oracle's ray asks about one
/// line. Where the subject carries a feature narrower than a texel — a
/// hair strand at this canvas is about one — the two make opposite
/// calls about whether it is there at all, and no sampling offset
/// reconciles them because the disagreement is not about *where* the
/// sample is taken. The shipped subject leaves one such point in
/// 192112 at the framing the easel develops at.
const SETTLED_BUDGET: f32 = 1e-4;

/// What fraction of the drawing's points may disagree at all.
///
/// A backstop, not the gate — [`marginal`] is the gate, and it is the
/// one that says something. This exists so a change that made *every*
/// verdict marginal (a bias collapsed to zero, a depth image an inch
/// deep) could not pass by satisfying the invariant vacuously. The
/// synthetic fixture below runs coarse on purpose — its bias is a fifth
/// of the form's radius, where the shipped subject's is a hundredth —
/// so its grazing band is wide and its share is far above anything the
/// real subject produces. The measured share is reported unfiltered at
/// every azimuth, so the budget can never hide a change.
const VERDICT_BUDGET: f32 = 0.02;

/// Ceiling on the mean absolute `reach` difference, in radians, over
/// the points where the GPU and the CPU derivation see the same
/// verdict. Both compute the same min-plus over the same arcs, so the
/// only honest source of difference is the probe's own quantization of
/// one part in 255 of [`REACH_WINDOW`].
const REACH_BUDGET: f32 = REACH_WINDOW / 255.0;

/// Ceiling on the worst per-curve `coverage` difference, in fraction.
/// Same argument: an exact count divided by an exact length, quantized
/// once by the probe.
const COVERAGE_BUDGET: f32 = 2.0 / 255.0;

fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// The test-only probe appended to the field module: the three planes
/// packed into one `Rgba8` texel so a single overlay rect carries all
/// of them out through the capture. Bindings are positional — input `n`
/// at `@binding(2 * n)` — and every read is a `textureLoad`, so the
/// paired samplers go undeclared.
const PROBE_WGSL: &str = r"
@group(1) @binding(0) var field_seen: texture_2d<f32>;
@group(1) @binding(2) var field_reach: texture_2d<f32>;
@group(1) @binding(4) var field_coverage: texture_2d<f32>;
@group(1) @binding(6) var field_reference: texture_2d<f32>;

@fragment
fn fs_probe(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let seen = textureLoad(field_seen, at, 0).r;
    let reach = textureLoad(field_reach, at, 0).r;
    let coverage = textureLoad(field_coverage, at, 0).r;
    let reference = textureLoad(field_reference, at, 0).r;
    let reference_drawn = select(0.0, 1.0, reference >= 0.0);
    let verdicts = (seen + reference_drawn * 2.0) / 3.0;

    return vec4<f32>(verdicts, min(reach / 0.02, 1.0), coverage, 1.0);
}
";

const FIELD_PROBE: u32 = sight::PLANE_COUNT as u32;

/// The field's own graph plus the probe: one more binding for the
/// `Rgba8` the probe writes, and one more pass reading the three planes
/// the graph just filled.
fn probed_program() -> ProgramRegister {
    let mut register = sight::program();
    register.wgsl = format!("{}\n{PROBE_WGSL}", register.wgsl);
    register.bindings.push(SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full });
    register.passes.push(ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_probe".to_owned(),
        inputs: vec![
            InputSlot::Binding { index: sight::SEEN },
            InputSlot::Binding { index: sight::REACH },
            InputSlot::Binding { index: sight::COVERAGE },
            InputSlot::Binding { index: sight::REFERENCE },
        ],
        output: OutputSlot::Binding { index: FIELD_PROBE },
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

/// The writable textures one dispatch writes into: the field's own
/// `R32Float` planes (nearest — `R32Float` refuses a filtering sampler,
/// and the values are quantities either way) and the probe's `Rgba8`.
fn create_targets(harness: &mut SubstrateHarness, width: usize, height: usize) -> Vec<u32> {
    const PLANE_LABELS: [&str; sight::PLANE_COUNT] =
        ["create_seen", "create_reach", "create_coverage", "create_total", "create_reference"];
    let (width, height) = (width as u32, height as u32);

    let mut targets: Vec<u32> = PLANE_LABELS
        .into_iter()
        .map(|label| {
            create_texture(
                harness,
                label,
                &CreateTexture {
                    width,
                    height,
                    format: TextureFormat::R32Float,
                    sampling: TextureSampling::Nearest,
                    usage: TextureUsage::Writable,
                    pixels: Vec::new(),
                },
            )
        })
        .collect();
    targets.push(create_texture(
        harness,
        "create_probe",
        &CreateTexture {
            width,
            height,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        },
    ));
    targets
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

/// Everything the field needs of a camera at one azimuth, as the puppet
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

/// The derived planes and reference verdict as the GPU wrote them,
/// decoded out of one capture and addressed by flat texel index the way
/// the field is.
struct Field {
    seen: Vec<bool>,
    reach: Vec<f32>,
    coverage: Vec<f32>,
    reference_drawn: Vec<bool>,
    /// Canvas height in texels — the scale one pixel of resampling is
    /// measured against when a disagreement is judged.
    height: usize,
}

/// Everything one dispatch names, mounted once and held. Kept together
/// because a dispatch names all of it and nothing here holds one
/// without the others.
struct Rig {
    program_id: u32,
    bindings: Vec<u32>,
    subject: u32,
    /// The two point buffers the field is rasterized from, in slot
    /// order: the resident half and the volatile one.
    points: [u32; 2],
    /// The curve-block buffer the reference and coverage reductions are
    /// rasterized from, re-pointed with the volatile half because its
    /// spans change with that drawing.
    curves: u32,
    width: usize,
    height: usize,
}

impl Rig {
    fn mount(
        harness: &mut SubstrateHarness,
        mesh: &Mesh,
        drawing: Drawing<'_>,
        layout: &Layout,
        eye: Vec3,
        canvas: (usize, usize),
        skin: Option<&deform::Skin>,
    ) -> Self {
        Self {
            program_id: register(harness, &probed_program()),
            bindings: create_targets(harness, canvas.0, canvas.1),
            subject: create_geometry(
                harness,
                "create_subject",
                &CreateGeometry {
                    layout: sight::subject_slot().layout,
                    vertices: sight::subject_vertices(mesh, skin),
                    indices: sight::subject_indices(mesh),
                },
            ),
            points: [
                create_geometry(
                    harness,
                    "create_resident_points",
                    &CreateGeometry {
                        layout: sight::points_slot().layout,
                        vertices: sight::point_vertices(
                            drawing,
                            layout.resident(),
                            skin.map(|skin| deform::Bound { rest: mesh, skin }),
                        ),
                        indices: sight::point_indices(layout.resident()),
                    },
                ),
                create_geometry(
                    harness,
                    "create_volatile_points",
                    &CreateGeometry {
                        layout: sight::posed_points_slot().layout,
                        vertices: sight::posed_point_vertices(drawing, layout.volatile()),
                        indices: sight::point_indices(layout.volatile()),
                    },
                ),
            ],
            curves: create_geometry(
                harness,
                "create_curves",
                &CreateGeometry {
                    layout: sight::curves_slot().layout,
                    vertices: sight::curve_vertices(drawing, layout, eye),
                    indices: sight::curve_indices(layout),
                },
            ),
            width: canvas.0,
            height: canvas.1,
        }
    }

    fn dispatch(
        &self,
        eye: Vec3,
        view_proj: Mat4,
        bias: f32,
        bones: &[f32; deform::BONE_LIMIT * 12],
    ) -> ProgramDispatch {
        ProgramDispatch {
            program_id: self.program_id,
            bindings: self.bindings.clone(),
            geometries: vec![self.subject, self.points[0], self.points[1], self.curves],
            // The parity oracle is `visibility::runs`, which gates
            // nothing on tone — the hatch gate is off, which is exactly
            // what an unrigged subject dispatches. The bone table is the
            // caller's: an unrigged arm passes the empty table, and the
            // bone-posed arm below passes `Skin::transforms` of its pose.
            uniforms: SightUniforms {
                view_proj,
                eye,
                field: (self.width as u32, self.height as u32),
                bias,
                bones: *bones,
                tone: ToneUniforms::of(&Settings::default(), false),
            }
            .encode(),
        }
    }

    /// One dispatch, plus the overlay and capture that carry its planes
    /// back. The drawing is already uploaded; the camera rides the
    /// uniform blob alone, which is the point — a turn costs the blob
    /// and no re-upload at all.
    fn read_field(
        &self,
        harness: &mut SubstrateHarness,
        eye: Vec3,
        view_proj: Mat4,
        bias: f32,
        bones: &[f32; deform::BONE_LIMIT * 12],
    ) -> Field {
        let probe = self.bindings[FIELD_PROBE as usize];
        let pre = vec![
            envelope("aether.render", &self.dispatch(eye, view_proj, bias, bones)),
            envelope("aether.render", &overlay(probe, self.width, self.height)),
        ];
        let captured =
            harness.execute(vec![("field", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture the field");
        let image = decode_png(captured.captured("field").expect("field capture ran")).expect("decode field png");

        let texels = self.width * self.height;
        let mut field = Field {
            seen: Vec::with_capacity(texels),
            reach: Vec::with_capacity(texels),
            coverage: Vec::with_capacity(texels),
            reference_drawn: Vec::with_capacity(texels),
            height: self.height,
        };
        for y in 0..self.height {
            for x in 0..self.width {
                let texel = rgba_at(&image, x as u32, y as u32);
                let verdicts = (srgb_byte_to_linear(texel[0]) * 3.0).round() as u8;
                field.seen.push(verdicts & 1 == 1);
                field.reach.push(srgb_byte_to_linear(texel[1]) * REACH_WINDOW);
                field.coverage.push(srgb_byte_to_linear(texel[2]));
                field.reference_drawn.push(verdicts >= 2);
            }
        }

        field
    }
}

/// Mount the rig, or re-point the one already mounted at a freshly
/// laid-out drawing. The subject never moves between eyes, and neither
/// does the resident half of the drawing — so only the volatile point
/// buffer is replaced, which is the shape the shipped path re-splits in
/// (iamacoffeepot/aether#4435).
fn re_point(
    harness: &mut SubstrateHarness,
    held: Option<Rig>,
    mesh: &Mesh,
    drawing: Drawing<'_>,
    layout: &Layout,
    eye: Vec3,
    canvas: (usize, usize),
) -> Rig {
    let Some(rig) = held else {
        return Rig::mount(harness, mesh, drawing, layout, eye, canvas, None);
    };

    harness
        .execute(vec![
            (
                "update_points",
                HarnessOp::send_and_settle(
                    "aether.render",
                    &UpdateGeometry {
                        geometry_id: rig.points[1],
                        vertices: sight::posed_point_vertices(drawing, layout.volatile()),
                        indices: sight::point_indices(layout.volatile()),
                    },
                ),
            ),
            (
                "update_curves",
                HarnessOp::send_and_settle(
                    "aether.render",
                    &UpdateGeometry {
                        geometry_id: rig.curves,
                        vertices: sight::curve_vertices(drawing, layout, eye),
                        indices: sight::curve_indices(layout),
                    },
                ),
            ),
        ])
        .expect("update_geometry sequence");

    rig
}

/// The derived fields for one curve.
struct Derived {
    reach: Vec<f32>,
    coverage: f32,
}

/// Which points of `curve` the eye can see, reconstructed from
/// `visibility::runs` at stride 1.
///
/// Taken from the split rather than transcribed, so the gate is against
/// the shipped function and not against a second spelling of it. The
/// split hands back runs of at least two points in order, each carrying
/// the original points, so walking the curve alongside the concatenated
/// runs recovers which points survived without asking any geometric
/// question twice.
///
/// A lone survivor between two hidden neighbours is dropped from the
/// runs and so reads as hidden here — which is the same rule [`derive`]
/// uses for coverage, because it is the same rule.
fn split_verdicts(mesh: &Mesh, eye: Vec3, curve: &Curve3, bias: f32) -> Vec<bool> {
    // The whole-or-nothing rule is a property of the *curve*, not of a
    // point's visibility, and the field carries it as a separate plane —
    // so it must not be folded into the verdict vector here. An
    // unauthored copy asks `runs` the same occlusion question with that
    // rule switched off.
    let asked = Curve3 { authored: false, ..curve.clone() };
    let runs = visibility::runs(mesh, eye, &asked, &|_| true, visibility::Mode::Opaque, 1, bias);

    let mut seen = vec![false; curve.points.len()];
    let mut at = 0;
    for point in runs.iter().flat_map(|run| &run.points) {
        while at < curve.points.len() && curve.points[at].pos != point.pos {
            at += 1;
        }
        if at < curve.points.len() {
            seen[at] = true;
            at += 1;
        }
    }

    seen
}

/// Arc between neighbouring points, in radians — `ribbon`'s own
/// measure, a world span over the distance to the eye. Index `i` is the
/// arc from point `i` to point `i + 1`, and the last is zero, which is
/// what puts a curve's own end at zero arc from its last point.
fn arc_steps(curve: &Curve3, eye: Vec3) -> Vec<f32> {
    curve
        .points
        .iter()
        .enumerate()
        .map(|(at, point)| {
            curve
                .points
                .get(at + 1)
                .map_or(0.0, |next| (next.pos - point.pos).length() / (point.probe - eye).length().max(1e-4))
        })
        .collect()
}

/// The derived fields for one curve, from a verdict vector.
///
/// `reach` restates the min-plus the scan runs: the least arc from each
/// point to a barrier — a point that is not seen, or either end of the
/// curve — inside the same window of `2^REACH_STEPS - 1` points the
/// scan resolves, saturating past it. `coverage` restates
/// `whole_or_nothing`'s own numerator: points surviving in runs of at
/// least two, over the curve's length.
fn derive(curve: &Curve3, eye: Vec3, seen: &[bool]) -> Derived {
    let points = curve.points.len();
    let window = (1i64 << sight::REACH_STEPS) - 1;
    let step = arc_steps(curve, eye);

    let reach = (0..points)
        .map(|at| {
            if !seen[at] {
                return 0.0;
            }
            let mut least = 1.0f32;

            // On past the point, to the first barrier — a hidden point
            // or, at index `points`, the empty texel the layout leaves
            // after every curve.
            let (mut arc, mut ahead) = (0.0f32, at + 1);
            while (ahead - at) as i64 <= window && ahead <= points {
                arc += step[ahead - 1];
                if ahead == points || !seen[ahead] {
                    least = least.min(arc);
                    break;
                }
                ahead += 1;
            }

            // And back, to the first barrier or to the empty texel
            // before the curve's first point.
            let (mut arc, mut behind) = (0.0f32, at as i64 - 1);
            while at as i64 - behind <= window {
                if behind < 0 {
                    least = least.min(arc);
                    break;
                }
                arc += step[behind as usize];
                if !seen[behind as usize] {
                    least = least.min(arc);
                    break;
                }
                behind -= 1;
            }

            least
        })
        .collect();

    let kept =
        (0..points).filter(|&at| seen[at] && (at > 0 && seen[at - 1] || at + 1 < points && seen[at + 1])).count();

    Derived { reach, coverage: kept as f32 / points as f32 }
}

/// A hidden point one texel beyond the old 31-point reach window is still
/// inside the angular pressure ramp at the far dolly. Missing it saturates
/// reach to the full-width value; the longer schedule must resolve it.
#[test]
fn the_far_dolly_scan_reaches_every_barrier_inside_the_pressure_ramp() {
    const POINTS: usize = 129;
    const TARGET: usize = 64;
    const BARRIER: usize = 32;
    const MEAN_POINT_EDGE: f32 = 0.0074;
    const FAR_DOLLY_DISTANCE: f32 = 40.0;

    let curve = Curve3 {
        points: (0..POINTS)
            .map(|at| {
                let x = (at as isize - TARGET as isize) as f32 * MEAN_POINT_EDGE;
                SurfacePoint::on_surface(Vec3::new(x, 0.0, 0.0), Vec3::new(0.0, 0.0, 1.0))
            })
            .collect(),
        class: FeatureClass::Silhouette,
        pen: Pen::Ink,
        seed: 0,
        authored: false,
    };
    let mut seen = vec![true; POINTS];
    seen[BARRIER] = false;

    let reach = derive(&curve, Vec3::new(0.0, 0.0, FAR_DOLLY_DISTANCE), &seen).reach[TARGET];

    assert_eq!(TARGET - BARRIER, (1 << 5), "the barrier sits just beyond the old window");
    assert!(reach > 0.0, "the barrier has nonzero arc from the target");
    assert!(reach < style::RAMP, "{reach:.6} rad should remain inside the pressure ramp");
}

/// The GPU's verdict for one curve, gathered out of the field.
fn gathered(field: &Field, span: &sight::Span) -> Vec<bool> {
    (0..span.len as usize).map(|at| field.seen[span.start as usize + at]).collect()
}

/// Whether the oracle's own verdict at one point is unstable across a
/// pixel of where the occluder is sampled — and so whether the field is
/// entitled to answer `wanted` instead.
///
/// The two mechanisms ask one question of one surface and differ only
/// in where they sample it: the oracle casts a ray through the probe,
/// the field reads the depth image at the centre of the pixel the probe
/// lands in. So the honest test of a disagreement is to move the probe
/// a pixel across the view and ask the oracle again — through the
/// oracle's own [`visibility::hidden`], not a transcription of it. A
/// point that answers the same from every neighbouring sample is not
/// describing a differently-sampled edge.
///
/// A pixel is measured at the point's own depth rather than at the
/// subject's, because a stroke at the back of the head projects through
/// a pixel that covers more of the world than one at the front.
fn marginal(mesh: &Mesh, eye: Vec3, bias: f32, curve: &Curve3, at: usize, wanted: bool, height: usize) -> bool {
    let point = curve.points[at];
    let grazes = matches!(curve.class, FeatureClass::Silhouette | FeatureClass::Decal);
    let to_eye = eye - point.probe;
    let depth = to_eye.length().max(1e-4);

    // The facing test is a dot product both sides evaluate the same
    // way, so it can only disagree within rounding of its own floor —
    // which is a tie-break of exactly the same kind.
    let facing = point.normal.dot(to_eye / depth);
    if (facing - 0.02).abs() <= 1e-5 {
        return true;
    }
    if !grazes && facing < 0.02 {
        return !wanted;
    }

    let pixel = depth * FIELD_OF_VIEW / height as f32;
    let across = (to_eye / depth).cross(Vec3::new(0.0, 1.0, 0.0)).normalize_or(Vec3::new(1.0, 0.0, 0.0));
    let up = (to_eye / depth).cross(across);

    (1..=SAMPLE_RINGS).any(|ring| {
        let reach = pixel * SAMPLE_PIXELS * ring as f32 / SAMPLE_RINGS as f32;
        (0..SAMPLE_SPOKES).any(|spoke| {
            let angle = TAU * spoke as f32 / SAMPLE_SPOKES as f32;
            let nudged =
                SurfacePoint { probe: point.probe + (across * angle.cos() + up * angle.sin()) * reach, ..point };

            visibility::hidden(mesh, eye, &nudged, bias) != wanted
        })
    })
}

/// Hold the GPU's verdicts against the oracle's, per point, over the
/// whole drawing. Returns how many points disagreed.
fn assert_verdicts(
    context: &str,
    mesh: &Mesh,
    eye: Vec3,
    bias: f32,
    drawing: Drawing<'_>,
    layout: &Layout,
    field: &Field,
) -> usize {
    let (mut differing, mut total, mut settled) = (0usize, 0usize, Vec::new());
    let (mut oracle_seen, mut field_seen) = (0usize, 0usize);
    for span in layout.spans() {
        let curve = drawing.curve(span.curve as usize).expect("a span names a curve of the drawing");
        let oracle = split_verdicts(mesh, eye, curve, bias);
        let measured = gathered(field, span);
        total += oracle.len();
        oracle_seen += oracle.iter().filter(|&&on| on).count();
        field_seen += measured.iter().filter(|&&on| on).count();

        for (at, (&expected, &actual)) in oracle.iter().zip(&measured).enumerate() {
            if expected == actual {
                continue;
            }
            differing += 1;

            if !marginal(mesh, eye, bias, curve, at, actual, field.height) {
                settled.push((span.id, at));
            }
        }
    }

    let share = differing as f32 / total.max(1) as f32;
    eprintln!(
        "{context}: {differing} of {total} points carry a different verdict ({:.4}% of the drawing); the oracle sees \
         {oracle_seen} and the field {field_seen}",
        100.0 * share,
    );
    // Without this the whole gate is passable in silence. A field that
    // wrote nothing at all agrees perfectly with an oracle that hid
    // everything, and every assertion below is satisfied by two blank
    // answers — which is exactly what a dropped dispatch, a mis-sized
    // binding or a mis-transcribed layout produces.
    for (name, count) in [("oracle", oracle_seen), ("field", field_seen)] {
        let share = count as f32 / total.max(1) as f32;
        assert!(
            (0.05..=0.95).contains(&share),
            "{context}: the {name} calls {:.1}% of the drawing visible, so this fixture is not asking the question — \
             a parity gate over an all-hidden or all-visible drawing agrees with anything",
            100.0 * share,
        );
    }
    let unexplained = settled.len() as f32 / total.max(1) as f32;
    eprintln!(
        "{context}: {} of them resolve under no resample ({:.4}% of the drawing)",
        settled.len(),
        100.0 * unexplained,
    );
    assert!(
        unexplained <= SETTLED_BUDGET,
        "{context}: {:.4}% of the drawing disagrees where the oracle's own verdict is settled across {SAMPLE_PIXELS} \
         pixels, past the {:.4}% budget, first at {:?} — a differently-sampled edge cannot move a verdict that \
         resampling does not move, so past a sub-texel feature or two this is a different occluder and not a \
         tie-break",
        100.0 * unexplained,
        100.0 * SETTLED_BUDGET,
        settled.first(),
    );
    assert!(
        share <= VERDICT_BUDGET,
        "{context}: {:.4}% of the drawing's points disagree, past the {:.4}% budget",
        100.0 * share,
        100.0 * VERDICT_BUDGET,
    );

    differing
}

/// Hold `reach` and `coverage` against the same derivation run on the
/// CPU over the GPU's own verdict plane, so a scan bug is measured
/// without the bias difference in the way.
fn assert_derived(context: &str, eye: Vec3, drawing: Drawing<'_>, layout: &Layout, field: &Field) {
    let (mut worst_reach, mut total_reach, mut counted) = (0.0f32, 0.0f32, 0usize);
    let mut worst_coverage = 0.0f32;

    for span in layout.spans() {
        let curve = drawing.curve(span.curve as usize).expect("a span names a curve of the drawing");
        let expected = derive(curve, eye, &gathered(field, span));
        assert_eq!(
            field.reference_drawn[span.start as usize],
            ribbon::reference_depth(curve, eye) >= 0.0,
            "{context}: curve {:?} carries the wrong reference-depth verdict",
            span.id,
        );

        for (at, &want) in expected.reach.iter().enumerate() {
            // Only inside the probe's encoded range: past it the store
            // saturates by design and the difference measured would be
            // the instrument's, not the scan's.
            if want >= REACH_WINDOW {
                continue;
            }
            let difference = (field.reach[span.start as usize + at] - want).abs();
            worst_reach = worst_reach.max(difference);
            total_reach += difference;
            counted += 1;
        }

        // Read at both ends of the span, not just one: the coverage is
        // one number gathered to every texel of the curve, and a
        // gather that reached the right total for the first point and
        // a neighbouring curve's for the last would pass a one-texel
        // check while telling half the drawing the wrong story.
        for at in [0, span.len as usize - 1] {
            let measured = field.coverage[span.start as usize + at];
            worst_coverage = worst_coverage.max((measured - expected.coverage).abs());
        }
    }

    let mean_reach = total_reach / counted.max(1) as f32;
    eprintln!(
        "{context}: reach drift — worst {worst_reach:.6}, mean {mean_reach:.6} rad over {counted} points; \
         coverage drift — worst {worst_coverage:.5}",
    );
    assert!(counted > 0, "{context}: no point carried a reach inside the probe's range, so nothing was measured");
    assert!(
        mean_reach <= REACH_BUDGET,
        "{context}: mean reach drift {mean_reach:.6} is past the {REACH_BUDGET:.6} rad budget — both sides run the \
         same min-plus over the same arcs, so nothing but the probe's quantization belongs here",
    );
    assert!(
        worst_coverage <= COVERAGE_BUDGET,
        "{context}: worst coverage drift {worst_coverage:.5} is past the {COVERAGE_BUDGET:.5} budget",
    );
}

/// Turn a point off every axis, so no feature of the fixture below
/// lands parallel to a pixel row or column.
fn turned(p: Vec3) -> Vec3 {
    let (sin_z, cos_z) = 0.27f32.sin_cos();
    let (sin_y, cos_y) = 0.42f32.sin_cos();
    let spun = Vec3::new(p.x * cos_z - p.y * sin_z, p.x * sin_z + p.y * cos_z, p.z);

    Vec3::new(spun.x * cos_y + spun.z * sin_y, spun.y, spun.z * cos_y - spun.x * sin_y)
}

/// The synthetic subject: a turned sphere with a slab standing across
/// one half of it.
///
/// Every property is load-bearing for what the parity claims. Round,
/// because the hatch level sets then run around the form and each
/// crosses the silhouette twice — so every curve carries both a
/// facing-test edge and an occlusion edge, which is what the verdict is
/// made of. Turned, so no edge is axis-aligned and the rasterizer's
/// fill rule and the oracle's ray cannot agree by construction. And the
/// slab, because self-occlusion alone would leave the field's hardest
/// case — one surface hiding a stroke on a *different* surface —
/// untested, which is the case the whole prepass exists for.
fn synthetic_subject(slab_shift: f32) -> Mesh {
    const RINGS: usize = 24;
    const SEGMENTS: usize = 32;

    let mut text = String::new();
    for ring in 0..=RINGS {
        let phi = PI * ring as f32 / RINGS as f32;
        for segment in 0..SEGMENTS {
            let theta = TAU * segment as f32 / SEGMENTS as f32;
            let at = turned(Vec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin()) * 0.7);
            writeln!(text, "v {} {} {}", at.x, at.y, at.z).expect("format vertex");
        }
    }
    // The slab stands in world coordinates rather than turned with the
    // sphere, so it is reliably between the eye and her front at the
    // framing the parity runs at. Its two long edges are slanted
    // anyway — no edge of this fixture may land parallel to a pixel
    // column, or the rasterizer's fill rule and the oracle's ray agree
    // by construction and the gate measures nothing.
    let slab = [
        Vec3::new(-0.95, -0.90, 1.15),
        Vec3::new(-0.05, -0.90, 1.30),
        Vec3::new(0.02, 0.90, 1.25),
        Vec3::new(-0.88, 0.90, 1.10),
    ];
    for corner in slab {
        writeln!(text, "v {} {} {}", corner.x + slab_shift, corner.y, corner.z).expect("format vertex");
    }

    let index = |ring: usize, segment: usize| ring * SEGMENTS + segment % SEGMENTS + 1;
    for ring in 0..RINGS {
        for segment in 0..SEGMENTS {
            let (a, b) = (index(ring, segment), index(ring, segment + 1));
            let (c, d) = (index(ring + 1, segment), index(ring + 1, segment + 1));
            // Wound so the face normals point out of the sphere. The
            // other way round the facing test rejects every hatch point
            // and the occlusion bias lifts every probe *into* the
            // subject, which hides the whole drawing — quietly, and
            // identically on both sides of the gate.
            writeln!(text, "f {a} {d} {c}").expect("format face");
            writeln!(text, "f {a} {b} {d}").expect("format face");
        }
    }
    let slab_base = (RINGS + 1) * SEGMENTS + 1;
    writeln!(text, "f {} {} {}", slab_base, slab_base + 1, slab_base + 2).expect("format face");
    writeln!(text, "f {} {} {}", slab_base, slab_base + 2, slab_base + 3).expect("format face");

    Mesh::from_obj_bytes(text.as_bytes(), RELAXATION).expect("synthetic subject")
}

/// The lighting and hatch density every run here works at. Spacing is
/// coarser than the shipped default so the fixture's drawing fits the
/// CI canvas's field — the layout would refuse it otherwise, which is
/// the refusal working, not a failure.
fn settings() -> Settings {
    Settings { hatch_spacing: 0.11, light: Vec3::new(0.3, 0.6, 1.0), ambient: 0.25, ..Settings::default() }
}

/// The drawing at one eye, kept as the two halves `Puppet::on_render`
/// keeps it in: the cached view-independent surface, then the charted
/// face, the suggestive contours and the silhouette.
///
/// Held apart rather than concatenated because that division is what
/// the field's layout is asked about — the resident half has to land on
/// the same texels at every eye, and a helper that handed over one list
/// could not state which curves those are.
struct Drawn {
    resident: Vec<Curve3>,
    volatile: Vec<Curve3>,
}

impl Drawn {
    fn as_drawing(&self) -> Drawing<'_> {
        Drawing { resident: &self.resident, volatile: &self.volatile }
    }
}

/// The whole drawing rather than a slice of it, because the authored
/// marks are the only curves the whole-or-nothing rule reaches — a
/// coverage plane held against an oracle over hatching alone would
/// never exercise the case it exists for.
fn drawing(mesh: &Mesh, settings: &Settings, labels: Option<&Labels>, eye: Vec3) -> Drawn {
    let anchors = labels.and_then(|labels| anchor::Anchors::measure(mesh, labels));
    let face = anchors
        .as_ref()
        .zip(settings.face)
        .map(|(anchors, face)| chart::marks(mesh, anchors, face, settings, eye))
        .unwrap_or_default();
    let drawn = |curves: Vec<Curve3>| curves.into_iter().filter(|curve| curve.points.len() >= 2).collect();

    Drawn {
        resident: drawn(extract::tone_gate(extract::surface(mesh, labels, anchors.as_ref(), settings), settings)),
        volatile: drawn(
            face.into_iter()
                .chain(extract::suggestive(mesh, mesh, labels, eye, settings))
                .chain(extract::silhouettes(mesh, eye))
                .collect(),
        ),
    }
}

/// Tripwire: the field says what the split says, at four eyes.
///
/// The bug this pins is the whole point of the slice. A field that
/// projects plausibly but biases, faces or depth-resolves differently
/// from the oracle would look perfectly reasonable as an image and
/// would cut the drawing in the wrong places the day the ink reads it —
/// strokes ending short of the edge they disappear behind, or running
/// on past it. Four eyes because one silhouette is one shape, and a
/// bias that happens to suit it is not a bias that suits her.
#[test]
fn the_gpu_field_splits_the_drawing_where_the_cpu_oracle_does() {
    if !require_wgpu_only() {
        return;
    }

    let mesh = synthetic_subject(0.0);
    let settings = settings();
    let bias = mesh.surface_bias();

    let mut harness = SubstrateHarness::builder()
        .size(CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32)
        .with_render()
        .build()
        .expect("harness");

    let mut mounted: Option<Rig> = None;
    for azimuth in AZIMUTHS {
        let (eye, view_proj) = camera(azimuth, CANVAS_WIDTH, CANVAS_HEIGHT);
        let drawn = drawing(&mesh, &settings, None, eye);
        let drawing = drawn.as_drawing();
        let layout = sight::layout(drawing, (CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32)).expect("the drawing fits");
        assert!(layout.points() > 500, "azimuth {azimuth}: a fixture this thin proves nothing");
        assert!(!layout.resident().is_empty() && !layout.volatile().is_empty(), "both halves carry curves");

        // The silhouette is a different curve at every eye, so the
        // volatile half is re-laid-out and re-uploaded per azimuth —
        // which is the re-split cadence the shipped path runs at. The
        // resident half is uploaded once, at the first azimuth, and the
        // verdicts below are read back through it at all four: a
        // resident span that moved with the volatile half would show up
        // here as the whole surface reading someone else's occlusion.
        let rig = re_point(&mut harness, mounted.take(), &mesh, drawing, &layout, eye, (CANVAS_WIDTH, CANVAS_HEIGHT));

        let field = rig.read_field(&mut harness, eye, view_proj, bias, &deform::bone_uniform(&[]));
        let context = format!("azimuth {azimuth}");
        assert_verdicts(&context, &mesh, eye, bias, drawing, &layout, &field);
        assert_derived(&context, eye, drawing, &layout, &field);
        mounted = Some(rig);
    }
}

/// Tripwire: the field follows vertices that move, through the
/// re-upload path and nothing else.
///
/// The standing requirement is that nothing is keyed on the subject
/// being the one from last frame — a pose is coming, and the whole
/// argument for moving occlusion onto the GPU is that a deforming
/// subject then occludes its own strokes with no index to rebuild. The
/// failure this catches is a prepass quietly holding the geometry it
/// first realized, so the depth image keeps describing a subject that
/// has already moved while every other assertion here still passes.
#[test]
fn a_re_uploaded_subject_re_occludes_from_its_new_vertices() {
    if !require_wgpu_only() {
        return;
    }

    let mesh = synthetic_subject(0.0);
    let settings = settings();
    let bias = mesh.surface_bias();
    let (eye, view_proj) = camera(0.0, CANVAS_WIDTH, CANVAS_HEIGHT);
    let drawn = drawing(&mesh, &settings, None, eye);
    let drawing = drawn.as_drawing();
    let layout = sight::layout(drawing, (CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32)).expect("the drawing fits");

    let mut harness = SubstrateHarness::builder()
        .size(CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32)
        .with_render()
        .build()
        .expect("harness");
    let rig = Rig::mount(&mut harness, &mesh, drawing, &layout, eye, (CANVAS_WIDTH, CANVAS_HEIGHT), None);
    let before = rig.read_field(&mut harness, eye, view_proj, bias, &deform::bone_uniform(&[]));

    // The pose: the slab swung across to the other side of her, so it
    // hides the strokes it was clearing and clears the ones it hid.
    // The drawing itself is untouched, so the field moving at all is
    // the signal.
    //
    // Built as a mesh rather than by moving the vertices of this one.
    // `Mesh` indexes its own triangles for the ray cast at construction,
    // so a `positions` written after the fact moves what the GPU
    // rasterizes and not what the oracle traverses — and the gate below
    // would then be holding a posed field against an unposed oracle.
    let posed = synthetic_subject(0.95);
    harness
        .execute(vec![(
            "update_subject",
            HarnessOp::send_and_settle(
                "aether.render",
                &UpdateGeometry {
                    geometry_id: rig.subject,
                    vertices: sight::subject_vertices(&posed, None),
                    indices: sight::subject_indices(&posed),
                },
            ),
        )])
        .expect("update_geometry sequence");

    let after = rig.read_field(&mut harness, eye, view_proj, bias, &deform::bone_uniform(&[]));
    let moved = before.seen.iter().zip(&after.seen).filter(|(was, now)| was != now).count();
    assert!(moved > 0, "a re-uploaded subject must re-occlude; not one of the field's texels moved");

    assert_verdicts("posed", &posed, eye, bias, drawing, &layout, &after);
    assert_derived("posed", eye, drawing, &layout, &after);
}

/// A little-endian, C-order `NumPy` 1.0 weight matrix.
fn weights_npy(values: &[f32], shape: (usize, usize)) -> Vec<u8> {
    assert_eq!(shape.0.checked_mul(shape.1), Some(values.len()), "fixture shape must match its values");
    let dictionary = format!("{{'descr': '<f4', 'fortran_order': False, 'shape': ({}, {}), }}", shape.0, shape.1);
    let padding = (16 - ((10 + dictionary.len() + 1) % 16)) % 16;
    let mut header = dictionary;
    header.extend(iter::repeat_n(' ', padding));
    header.push('\n');

    let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
    bytes.extend(u16::try_from(header.len()).expect("a short header").to_le_bytes());
    bytes.extend(header.as_bytes());
    bytes.extend(values.iter().flat_map(|value| value.to_le_bytes()));

    bytes
}

/// The subject rebuilt at the positions the CPU skin sent it. `Mesh`
/// indexes its triangles for the ray cast at construction, so the posed
/// oracle must be *built* posed — positions written after the fact move
/// what the GPU rasterizes and not what the oracle traverses.
fn obj_of(positions: &[Vec3], faces: &[[u32; 3]]) -> Mesh {
    let mut text = String::new();
    for at in positions {
        writeln!(text, "v {} {} {}", at.x, at.y, at.z).expect("format vertex");
    }
    for face in faces {
        writeln!(text, "f {} {} {}", face[0] + 1, face[1] + 1, face[2] + 1).expect("format face");
    }

    Mesh::from_obj_bytes(text.as_bytes(), RELAXATION).expect("posed oracle mesh")
}

/// Tripwire: the field follows a subject the *bone table* poses, held
/// against an oracle ray-casting the same pose.
///
/// Every other dispatch in this file carries the empty bone table, so
/// the prepass' `skin_point`, the point stage's `anchored_point` and
/// the `bone_row` indexing of the uniform window ship on the strength
/// of rest arms alone. What this catches is any of them disagreeing
/// with `Skin::transforms` about where a pose sends the occluder — a
/// transposed row, a mis-strided window, a share lane off by one —
/// failures no rest arm can see, and which present live as verdicts
/// flickering while the subject moves.
#[test]
fn a_bone_posed_occluder_occludes_where_the_posed_oracle_says() {
    if !require_wgpu_only() {
        return;
    }

    let mesh = synthetic_subject(0.0);
    let settings = settings();
    let bias = mesh.surface_bias();
    let (eye, view_proj) = camera(0.0, CANVAS_WIDTH, CANVAS_HEIGHT);
    let drawn = drawing(&mesh, &settings, None, eye);
    let drawing = drawn.as_drawing();
    let layout = sight::layout(drawing, (CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32)).expect("the drawing fits");

    // The rig: the sphere rides a bone no pose ever drives, so the
    // drawing's geometry holds still and the reference depths stay the
    // rest solve's; the slab rides the head, so the pose swings the
    // occluder and nothing else. One-hot weights, so the u8 share lane
    // is exact and the arm measures posing rather than quantization.
    let vertices = mesh.positions.len();
    let mut weights = vec![0.0f32; vertices * 2];
    for (vertex, row) in weights.chunks_exact_mut(2).enumerate() {
        row[usize::from(vertex >= vertices - 4)] = 1.0;
    }
    let descriptor = "bones chest head\npivot head -0.95 0.0 1.2";
    let skin = deform::Skin::parse(&weights_npy(&weights, (vertices, 2)), descriptor, vertices).expect("the rig binds");

    let mut harness = SubstrateHarness::builder()
        .size(CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32)
        .with_render()
        .build()
        .expect("harness");
    let rig = Rig::mount(&mut harness, &mesh, drawing, &layout, eye, (CANVAS_WIDTH, CANVAS_HEIGHT), Some(&skin));

    // At rest the table is live but every transform is the identity, so
    // the skinning path must reproduce the rest arm exactly.
    let rest = deform::bone_uniform(&skin.transforms(&Pose::default()));
    let before = rig.read_field(&mut harness, eye, view_proj, bias, &rest);
    assert_verdicts("bone-rest", &mesh, eye, bias, drawing, &layout, &before);
    assert_derived("bone-rest", eye, drawing, &layout, &before);

    // The pose: a head yaw swings the slab about its own left edge, so
    // it hides strokes it was clearing and clears strokes it hid while
    // staying over the page.
    let pose = Pose { yaw: 25.0, ..Pose::default() };
    let transforms = skin.transforms(&pose);
    let (mut positions, mut normals) = (mesh.positions.clone(), mesh.normals.clone());
    skin.pose_surface(&transforms, &mesh, &mut positions, &mut normals);
    let oracle = obj_of(&positions, &mesh.faces);

    let after = rig.read_field(&mut harness, eye, view_proj, bias, &deform::bone_uniform(&transforms));
    let moved = before.seen.iter().zip(&after.seen).filter(|(was, now)| was != now).count();
    assert!(moved > 0, "a bone-posed occluder must re-occlude; not one of the field's texels moved");

    assert_verdicts("bone-posed", &oracle, eye, bias, drawing, &layout, &after);
    assert_derived("bone-posed", eye, drawing, &layout, &after);
}

/// The shipped subject, at the size and framing the easel develops at.
const CROSSFEED_WIDTH: usize = 900;
const CROSSFEED_HEIGHT: usize = 1200;

/// How many frames each condition of the cost measurement averages
/// over, and how many lead frames are thrown away first.
const COST_SAMPLES: u32 = 40;
const COST_WARMUP: u32 = 8;

/// Cross-feed instrument: the field against `visibility::runs` on the
/// real subject at four azimuths, reporting the disagreement and the
/// field's own per-frame cost.
///
/// Ignored by default — it needs a 434k-face mesh and its material
/// field, which live outside the repository:
///
/// ```text
/// AETHER_CROSSFEED_DIR=/path/to/dir \
///     cargo test -p aether-puppet --release --test program_sight_scenario \
///     -- --ignored --nocapture
/// ```
///
/// where the directory holds `subject.obj` and `labels.npy`.
///
/// The cost is timed across `advance` frames and never across a
/// capture. The harness capture encodes a PNG synchronously and its
/// cost is priced by the image's entropy, which is nothing to do with
/// the dispatch and swamps it (iamacoffeepot/aether#4422).
#[test]
#[ignore = "instrument; needs the shipped subject in AETHER_CROSSFEED_DIR"]
fn crossfeed_the_gpu_field_against_the_cpu_oracle() {
    if !require_wgpu_only() {
        return;
    }
    let Ok(dir) = env::var("AETHER_CROSSFEED_DIR") else {
        eprintln!("AETHER_CROSSFEED_DIR unset; nothing to cross-feed");
        return;
    };
    let dir = Path::new(&dir);

    let mesh = Mesh::from_obj_bytes(&fs::read(dir.join("subject.obj")).expect("read subject.obj"), RELAXATION)
        .expect("parse the subject");
    let labels =
        Labels::decode(&fs::read(dir.join("labels.npy")).expect("read labels.npy"), mesh.min, mesh.max, LABEL_PAD)
            .expect("parse the material field");
    let settings = Settings::default();
    let bias = mesh.surface_bias();
    eprintln!("crossfeed: {} faces, {} vertices, surface bias {bias:.5}", mesh.faces.len(), mesh.positions.len());

    let mut harness = SubstrateHarness::builder()
        .size(CROSSFEED_WIDTH as u32, CROSSFEED_HEIGHT as u32)
        .with_render()
        .build()
        .expect("harness");

    let mut mounted: Option<Rig> = None;
    for azimuth in AZIMUTHS {
        let (eye, view_proj) = camera(azimuth, CROSSFEED_WIDTH, CROSSFEED_HEIGHT);
        let drawn = drawing(&mesh, &settings, Some(&labels), eye);
        let drawing = drawn.as_drawing();
        let layout = sight::layout(drawing, (CROSSFEED_WIDTH as u32, CROSSFEED_HEIGHT as u32)).expect("fits");
        eprintln!(
            "crossfeed azimuth {azimuth}: {} curves ({} resident), {} points ({} resident), {} of {} texels occupied",
            layout.spans().len(),
            layout.resident().len(),
            layout.points(),
            layout.resident().iter().map(|span| span.len as usize).sum::<usize>(),
            layout.occupied(),
            CROSSFEED_WIDTH * CROSSFEED_HEIGHT,
        );

        let started = Instant::now();
        for curve in drawing.curves() {
            visibility::runs(&mesh, eye, curve, &|_| true, visibility::Mode::Opaque, 1, bias);
        }
        let oracle_millis = started.elapsed().as_secs_f64() * 1000.0;

        let rig =
            re_point(&mut harness, mounted.take(), &mesh, drawing, &layout, eye, (CROSSFEED_WIDTH, CROSSFEED_HEIGHT));

        let field = rig.read_field(&mut harness, eye, view_proj, bias, &deform::bone_uniform(&[]));
        let context = format!("crossfeed azimuth {azimuth}");
        assert_verdicts(&context, &mesh, eye, bias, drawing, &layout, &field);
        assert_derived(&context, eye, drawing, &layout, &field);
        eprintln!("{context}: the CPU oracle split the drawing in {oracle_millis:.1} ms at stride 1");

        if azimuth == 0.0 {
            report_cost(&mut harness, &rig, eye, view_proj, bias);
        }
        mounted = Some(rig);
    }
}

/// The field's per-frame cost: a run of frames with the dispatch
/// mailed against a run without, differenced.
///
/// Two properties of the machinery decide the shape of this, and both
/// of them break the obvious measurement.
///
/// The render runtime runs one frame in flight: it waits on the
/// previous submission at the top of the next frame, so a frame's GPU
/// cost is billed to the *following* frame's wall clock. Alternating
/// one dispatched frame with one bare frame therefore reports each
/// condition's cost under the other's name — measured that way, the
/// bare frame appeared to cost 8.77 ms and the dispatched one 4.36 ms,
/// and the difference arrived negative. Each condition runs as a
/// consecutive run instead, which amortizes the lag to one frame in
/// [`COST_SAMPLES`], and the bare run is taken on both sides of the
/// dispatched one: a bare run that only follows a dispatched one still
/// drains it and reads five times too high, and having both numbers is
/// what makes that visible rather than silently doubling the answer.
///
/// And nothing is timed through a capture. The harness capture encodes
/// a PNG synchronously and its cost is priced by the image's entropy —
/// 37.9 ms for a dense field against 1.9 ms for a flat fill over the
/// same geometry — which is nothing to do with the dispatch and swamps
/// it (iamacoffeepot/aether#4422).
fn report_cost(harness: &mut SubstrateHarness, rig: &Rig, eye: Vec3, view_proj: Mat4, bias: f32) {
    // Time the production graph, not the readback probe appended by this
    // scenario. The probe is a full-field pass of its own and adding a
    // fourth observed plane would otherwise look like a field regression.
    let mut dispatch = rig.dispatch(eye, view_proj, bias, &deform::bone_uniform(&[]));
    dispatch.program_id = register(harness, &sight::program());
    dispatch.bindings.truncate(sight::PLANE_COUNT);

    let mut run = |dispatched: bool| {
        let mut millis = 0.0;
        for sample in 0..COST_SAMPLES + COST_WARMUP {
            let started = Instant::now();
            let ops = if dispatched {
                vec![
                    ("dispatch", HarnessOp::send_and_settle("aether.render", &dispatch)),
                    ("frame", HarnessOp::advance(1)),
                ]
            } else {
                vec![("frame", HarnessOp::advance(1))]
            };
            harness.execute(ops).expect("timed frame");

            // The warm-up pays for the geometry's first realization on
            // the GPU and for the one-frame lag to fill, neither of
            // which is a per-frame cost.
            if sample >= COST_WARMUP {
                millis += started.elapsed().as_secs_f64() * 1000.0;
            }
        }

        millis / f64::from(COST_SAMPLES)
    };

    let before = run(false);
    let with = run(true);
    let after = run(false);
    let without = before.min(after);
    eprintln!(
        "crossfeed: the field costs {:.2} ms per frame at {CROSSFEED_WIDTH}x{CROSSFEED_HEIGHT} ({with:.2} ms \
         dispatched against {without:.2} ms bare — {before:.2} before and {after:.2} after — over {COST_SAMPLES} \
         consecutive frames each)",
        with - without,
    );
}
