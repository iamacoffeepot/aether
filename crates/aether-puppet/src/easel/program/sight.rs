//! Stroke visibility as a field over each stroke's own parameterization
//! (iamacoffeepot/aether#4418, ADR-0172).
//!
//! [`crate::visibility::runs`] answers, per point, whether the eye can
//! see it — one ray each, against a BVH over 434k faces, every time the
//! eye moves. That walk is the frame budget's binding constraint, and
//! it is also the piece that scales worst into animation, where every
//! frame is a re-split and the index the rays traverse would have to be
//! rebuilt with the pose.
//!
//! The structural fact this rests on is that **the subject is never
//! drawn**: the picture is ink over the wash sheet, and the mesh exists
//! only as the thing they are about. So occlusion is not a question
//! about the frame at all — it is a question a program can answer
//! inside its own pass graph, against a depth image of a mesh nobody
//! sees.
//!
//! The planes that come out carry, between them, everything the CPU
//! run-splitter and the CPU rail solve produced:
//!
//! - [`SEEN`] — the verdict per point, the same conjunction
//!   `visibility::drawn` reaches.
//! - [`REACH`] — arc to the nearest hidden point or curve end, in
//!   radians. Exactly what [`crate::style::pressure`] tapers by, so a
//!   stroke thins into an occluder instead of cutting hard.
//! - [`COVERAGE`] — the fraction of each curve that survives in runs of
//!   at least two points, carried to every one of its texels. That is
//!   the quantity `visibility::whole_or_nothing` divides, so the
//!   whole-or-nothing rule an authored mark passes (`CHART_COVERAGE`)
//!   becomes a comparison against this plane.
//! - [`REFERENCE`] — one texel per curve rather than per point:
//!   the stroke's own average distance to the eye, with the sign carrying
//!   whether the curve is drawn at this eye at all. A sparse draw reduces
//!   it from the point depths already on the GPU. That is everything the
//!   eye decides about a curve rather than about one of its points, and
//!   delivering it here is what lets the ink pass solve its rails in the
//!   vertex stage from a ribbon buffer that never travels
//!   (iamacoffeepot/aether#4440).
//!
//! # What this module is not
//!
//! It does not draw. The planes are read by [`super::stroke`], whose
//! vertex stage folds them into stroke widths, and this side's own
//! answers are held against `visibility::runs` by
//! `tests/program_sight_scenario.rs`.
//!
//! # The field's layout, and why it has no rows
//!
//! ADR-0172 describes a curve owning a row of the field. Measured on
//! the shipped drawing, a row is the wrong unit — the point count per
//! curve spans nearly three orders of magnitude:
//!
//! | | curves | points | p50 | p90 | p99 | max |
//! |---|---|---|---|---|---|---|
//! | azimuth 0 | 4124 | 192112 | 15 | 104 | 488 | 3150 |
//! | azimuth 90 | 4241 | 203829 | 14 | 100 | 471 | 11017 |
//!
//! The median curve is fifteen points and the longest silhouette is
//! eleven thousand. A fixed row width `W` costs `curves * W` texels and
//! truncates everything past `W`: at `W = 256` the field is already
//! 1.11M texels — past the 1.08M a 900x1200 canvas affords — and 160
//! curves still overflow; holding the longest curve needs `W = 16384`
//! and 68M texels, sixty times the capacity. Neither branch of the
//! usual policy survives that. Splitting a long curve across rows puts
//! a false barrier mid-stroke, and the reach scan reads a barrier as a
//! stroke end, so the drawing's most important line — the outline —
//! would taper to nothing in its own middle. Capping with an error
//! drops that line outright.
//!
//! So the field has no rows. Curves pack end to end into the flat texel
//! index, each preceded by one empty texel, and a "row" is a span. The
//! measured cost is `points + curves + 1` texels — 196k at azimuth 0
//! against a 1.08M capacity, five times over — and no curve is ever
//! split or truncated. What is capped is one reduction's workload: a
//! sparse per-curve draw reduces up to [`MAX_CURVE_POINTS`] points
//! exactly, and [`layout`] refuses a longer one by name rather than
//! silently asking one fragment invocation to run without bound.
//!
//! The empty texel is not padding. It reads as hidden and carries zero
//! arc, so it is the barrier that ends a curve at zero arc from its own
//! last point — which is what the reach scan has to mean at an end.
//!
//! # Why the whole graph is one program
//!
//! A program's slots all resolve from one reference extent by integer
//! division (`SlotExtent::Divided`), so a canvas-resolution depth image
//! and a field of some unrelated size cannot share a dispatch, and two
//! programs cannot share a texture whose size does not divide the
//! other's. The field is therefore the canvas's own extent — which is
//! also why its capacity is stated in canvas texels above.

use aether_math::{Mat4, Vec3};
use aether_render::{
    DrawPass, GeometrySlotSpec, InputSlot, OutputSlot, PassLoad, PassStage, ProgramPass, ProgramRegister, SlotExtent,
    SlotSpec, TextureFormat, VertexAttribute, VertexFormat,
};

use super::{SKIN_WGSL, TONE_WGSL};
use crate::deform::{Anchored, BONE_LIMIT, Bound, INFLUENCES, Skin};
use crate::extract::Settings;
use crate::feature::{Curve3, Drawing, FeatureClass};
use crate::math3::hash64;
use crate::mesh::Mesh;
use crate::{ribbon, weld};

/// The field's own WGSL: the prepass, point and curve rasterization, and
/// the reach scan.
pub const SIGHT_WGSL: &str = include_str!("sight.wgsl");

/// Dispatch-binding indices, in the order [`program`] declares them.
/// Each is a full-extent `R32Float` plane, so a verdict stays the
/// integer it is and an arc stays the radian it is.
pub const SEEN: u32 = 0;
pub const REACH: u32 = 1;
pub const COVERAGE: u32 = 2;
pub const TOTAL: u32 = 3;
pub const REFERENCE: u32 = 4;

/// How many plane bindings a dispatch supplies.
pub const PLANE_COUNT: usize = 5;

/// Geometry slot indices: the subject the prepass rasterizes, then the
/// drawing's points in two buffers, then bounded reduction blocks.
///
/// The points divide by volatility rather than by class
/// ([`Drawing`]). [`RESIDENT`] carries the
/// curves that do not depend on the eye and travels once per subject;
/// [`VOLATILE`] carries the rest and travels per re-split. The two
/// occupy disjoint texels, so the passes that rasterize them write into
/// one plane with the second loading what the first laid down
/// (iamacoffeepot/aether#4435).
///
/// [`CURVES`] is neither: it carries one compact record per reduction
/// block rather than per point. Dividing it would add another dispatch
/// geometry without changing which drawing updates its spans.
pub const SUBJECT: u32 = 0;
pub const RESIDENT: u32 = 1;
pub const VOLATILE: u32 = 2;
pub const CURVES: u32 = 3;

/// How many geometry slots a dispatch supplies.
pub const GEOMETRY_COUNT: usize = 4;

/// Doublings the reach scan runs, so it resolves any barrier within
/// `2^REACH_STEPS - 1` points and saturates past it.
///
/// Thirty-one points. The taper it feeds ramps over
/// `style::pressure`'s `RAMP` of 0.0064 radians and is flat beyond, and
/// a point of the shipped drawing is about 0.0014 radians of arc from
/// the next (a 0.0074 mean edge at the framing's 5.4 distance) — so the
/// ramp is under five points long and the window clears it six times
/// over. Past the window every arc is far enough that the taper reads
/// the same, which is what makes saturating honest rather than lossy.
pub const REACH_STEPS: u32 = 5;

/// Longest curve one sparse reduction invocation is allowed to walk,
/// and so the longest [`layout`] admits.
///
/// Kept at the former exact-scan limit to preserve the field's accepted
/// input surface. The measured longest curve is 11017 points, while this
/// admits 32767 and makes the shader loop's upper bound explicit.
pub const MAX_CURVE_POINTS: usize = (1 << 15) - 1;

/// Points one bounded curve-reduction fragment folds.
///
/// The second level walks only these block totals. On the shipped
/// drawing that changes the worst fragment from 11017 texture loads to
/// 345, while the first level exposes every 32-point block in parallel.
const CURVE_BLOCK_POINTS: usize = 32;

/// Transient indices, in the order [`program`] declares them. The pairs
/// are the reach scan's ping-pong: a pass may not read the slot it
/// writes, so each doubling reads one and writes the other.
const DEPTH: u32 = 0;
const STEP: u32 = 1;
const HEAD: u32 = 2;
const POINT_DEPTH: u32 = 3;
const BLOCK_DEPTH: u32 = 4;
const BLOCK_ARC: u32 = 5;
const BLOCK_COVERAGE: u32 = 6;
const CURVE_COVERAGE: u32 = 7;
const ARC: [u32; 2] = [8, 9];
const HEAD_SPREAD: [u32; 2] = [10, 11];
const TAIL_SPREAD: [u32; 2] = [12, 13];
const TRANSIENT_COUNT: usize = 14;

/// One plane's slot: full-extent `R32Float`, the format the wash's own
/// data planes already ride (ADR-0170) and the only one that carries an
/// arc without quantizing it.
#[must_use]
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// The prepass' geometry: a rest position and the bone binding that
/// poses it.
///
/// Nothing else is rasterized — the pass writes distance to the eye and
/// tests depth, and the point that reads it back needs no shading. The
/// pose reaches the stage through the uniform blob instead of through
/// the buffer, so the depth image is of this frame's pose while the
/// buffer is uploaded once and never travels again
/// (iamacoffeepot/aether#4462).
#[must_use]
pub fn subject_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Uint8x4 },
            VertexAttribute { location: 2, format: VertexFormat::Unorm8x4 },
        ],
    }
}

/// Bytes one subject vertex occupies: the rest position, then the four
/// joint indices and the four shares.
pub const SUBJECT_VERTEX_BYTES: usize = 12 + 4 + 4;

/// The drawing's points, one texel-sized triangle each.
///
/// The first eight lanes are the point's *address on the sculpt*: two
/// corners of the face it was found in, each carrying a rest position, a
/// rest normal and its own bone binding — the `Uint8x4` joint indices
/// and `Unorm8x4` shares the closed vertex format set names for skinning.
/// `between` at the end says where between the two corners the point
/// sits. That is what lets the pose ride the uniform blob: the vertex
/// stage poses both corners and interpolates the results, and the buffer
/// is the rest sculpt's, uploaded once (iamacoffeepot/aether#4462).
///
/// `slot` is the point's flat field index shifted up two bits with its
/// corner in the low pair, because a vertex stage has no way to ask
/// which corner it is: `@builtin(vertex_index)` under an indexed draw
/// is the index *value*, which three corners of one point would share.
///
/// The two pairs after it are `ends` — points back to the curve's first
/// and on to its last — and `stroke` — the world span to the next point,
/// and the curve's class as a code: negative where the class grazes the
/// eye by definition (a silhouette or a decal, which neither the facing
/// test nor the tone gate may reach), and otherwise the hatch family's
/// own level, which is the threshold the gate compares against.
///
/// Everything here is view-independent *and* pose-independent — the arc
/// a stroke spans is carried as a world length and divided by the camera
/// in the shader, and the corners are the rest sculpt's. So this buffer
/// is re-uploaded when the drawing is re-extracted and neither when the
/// eye moves nor when the subject poses.
#[must_use]
pub fn points_slot() -> GeometrySlotSpec {
    let corner = |location: u32| {
        [
            VertexAttribute { location, format: VertexFormat::Float32x3 },
            VertexAttribute { location: location + 1, format: VertexFormat::Float32x3 },
            VertexAttribute { location: location + 2, format: VertexFormat::Uint8x4 },
            VertexAttribute { location: location + 3, format: VertexFormat::Unorm8x4 },
        ]
    };
    let mut layout = corner(0).to_vec();
    layout.extend(corner(4));
    layout.extend([
        VertexAttribute { location: 8, format: VertexFormat::Float32 },
        VertexAttribute { location: 9, format: VertexFormat::Float32x2 },
        VertexAttribute { location: 10, format: VertexFormat::Float32x2 },
        VertexAttribute { location: 11, format: VertexFormat::Float32 },
    ]);

    GeometrySlotSpec { layout }
}

/// Bytes one point vertex occupies: two corner bindings, then slot, ends,
/// stroke and the share between the corners. Attributes pack in
/// declaration order with no padding (ADR-0171), so this is the sum of
/// the formats' widths and [`point_vertices`] writes them in exactly
/// this order.
pub const POINT_VERTEX_BYTES: usize = 2 * (12 + 12 + 4 + 4) + 4 + 8 + 8 + 4;

/// The same points with no binding: where the point already *is*, rather
/// than the address a vertex stage would pose it from.
///
/// The volatile half is re-solved on the CPU every frame it changes, at
/// whatever pose is running, so it arrives posed and an anchorage would
/// be two corners of a face it no longer sits on. Carrying one anyway
/// costs it the binding's bytes — and the volatile half is the buffer
/// that travels, so those bytes are per frame rather than per subject.
/// Hence two slots and two vertex entry points over one fragment stage:
/// [`RESIDENT`] is anchored on the sculpt, [`VOLATILE`] stands for
/// itself.
#[must_use]
pub fn posed_points_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 2, format: VertexFormat::Float32 },
            VertexAttribute { location: 3, format: VertexFormat::Float32x2 },
            VertexAttribute { location: 4, format: VertexFormat::Float32x2 },
        ],
    }
}

/// Bytes one already-posed point vertex occupies: probe, normal, slot,
/// ends and stroke.
pub const POSED_POINT_VERTEX_BYTES: usize = 12 + 12 + 4 + 8 + 8;

/// Uniform window for every pass — the WGSL `SightParams` block.
///
/// One block is laid down per scan doubling, each carrying its own
/// `stride`, and a pass windows the copy that carries its. The camera
/// half is identical in all of them: it is the whole per-frame state,
/// which is the point — a turn changes this blob and nothing else.
pub struct SightUniforms {
    /// The matrix the drawing was solved for, so a point projects into
    /// the depth image the way the subject rasterized into it.
    pub view_proj: Mat4,
    /// Where the viewer sits — the distance every occlusion test and
    /// every arc is measured against.
    pub eye: Vec3,
    /// Field size in texels. The reference extent, so also the depth
    /// image's size.
    pub field: (u32, u32),
    /// How far a point is lifted off the surface before the occlusion
    /// question is asked — [`Mesh::surface_bias`] of the mesh the
    /// *point* came from, which is why it is supplied rather than
    /// derived (`visibility::runs`).
    pub bias: f32,
    /// This frame's pose, as [`deform::bone_uniform`] lays it out.
    ///
    /// The whole of what a pose costs the frame. Every curve's geometry
    /// is the rest sculpt's and stays on the GPU; what changes when she
    /// turns her head is three hundred and eighty-four bytes.
    ///
    /// [`deform::bone_uniform`]: crate::deform::bone_uniform
    pub bones: [f32; BONE_LIMIT * 12],
    /// The tone gate's authored numbers: the key light's direction, the
    /// shading floor, the face lift, and the three hatch thresholds.
    pub tone: ToneUniforms,
}

/// What [`Settings::tone`] and the hatch gate are made of, carried into
/// the shader that now has to answer them.
///
/// [`Settings::tone`]: crate::extract::Settings::tone
#[derive(Clone, Copy)]
pub struct ToneUniforms {
    pub light: Vec3,
    pub ambient: f32,
    pub thresholds: [f32; 3],
    pub face_lift: f32,
    /// Whether the shader gates hatching at all.
    ///
    /// One or zero, and it is which side of the pose the gate stands on
    /// rather than a preference. A subject with no rig turns no normals,
    /// so its hatching is gated once at load and arrives here already
    /// split into lit runs — gating a second time would re-decide a
    /// settled question through a different `sin`. A rigged subject
    /// carries its surface curves ungated precisely because the normals
    /// the gate reads are the ones this stage just posed.
    pub gate: bool,
}

impl ToneUniforms {
    /// The gate, read off the settings a subject was extracted with.
    #[must_use]
    pub fn of(settings: &Settings, gate: bool) -> Self {
        Self {
            light: settings.light,
            ambient: settings.ambient,
            thresholds: settings.hatch_thresholds,
            face_lift: settings.face_lift,
            gate,
        }
    }
}

impl SightUniforms {
    /// Bytes of one `SightParams` block: a `mat4x4<f32>`, the camera and
    /// field scalars, the tone block, and the bone table — rounded to
    /// the struct's 16-byte alignment.
    pub const BYTES: u32 = 144 + (BONE_LIMIT * 48) as u32;

    /// Where the tone block starts inside one window.
    const TONE: usize = 96;

    /// Where the bone table starts inside one window. Sixteen-aligned,
    /// as an array of `vec4<f32>` must be.
    const BONES: usize = 144;

    /// How many copies the blob carries — one per reach doubling.
    pub const WINDOWS: u32 = REACH_STEPS;

    /// Byte offset of the window whose `stride` is `2^step`.
    #[must_use]
    pub const fn window(step: u32) -> u32 {
        Self::BYTES * step
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut blob = vec![0u8; (Self::BYTES * Self::WINDOWS) as usize];
        for step in 0..Self::WINDOWS {
            let at = Self::window(step) as usize;
            let window = &mut blob[at..at + Self::BYTES as usize];
            for (lane, value) in window[0..64].chunks_exact_mut(4).zip(self.view_proj.to_cols_array()) {
                lane.copy_from_slice(&value.to_le_bytes());
            }
            for (lane, value) in window[64..76].chunks_exact_mut(4).zip(self.eye.to_array()) {
                lane.copy_from_slice(&value.to_le_bytes());
            }
            let tail = [self.field.0 as f32, self.field.1 as f32, self.bias, (1u32 << step) as f32];
            for (lane, value) in window[80..96].chunks_exact_mut(4).zip(tail) {
                lane.copy_from_slice(&value.to_le_bytes());
            }

            // `light` is a `vec3<f32>` at a sixteen-byte boundary, so
            // `ambient` fills its padding lane; `thresholds` is the next
            // `vec3` and starts the following boundary.
            let lit = [
                self.tone.light.x,
                self.tone.light.y,
                self.tone.light.z,
                self.tone.ambient,
                self.tone.thresholds[0],
                self.tone.thresholds[1],
                self.tone.thresholds[2],
                self.tone.face_lift,
                f32::from(u8::from(self.tone.gate)),
            ];
            for (lane, value) in window[Self::TONE..].chunks_exact_mut(4).zip(lit) {
                lane.copy_from_slice(&value.to_le_bytes());
            }
            for (lane, value) in window[Self::BONES..].chunks_exact_mut(4).zip(self.bones) {
                lane.copy_from_slice(&value.to_le_bytes());
            }
        }

        blob
    }
}

/// A curve's identity, independent of where it landed in the drawing.
///
/// Derived from what the curve *is* — its feature seed, its class, and
/// its two welded endpoints on the weld's own quantisation grid — never
/// from traversal order. The endpoints are sorted before hashing, so a
/// curve welded from the far end is the same curve. That is what lets a
/// span be addressed by identity: the pack orders by this, so the same
/// drawing lays out the same way however the extractor happened to
/// enumerate it, and a curve that changes shape (the silhouette does,
/// every frame) announces itself with a new id rather than quietly
/// inheriting the old one's span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CurveId(pub u64);

fn class_code(class: FeatureClass) -> u64 {
    match class {
        FeatureClass::Silhouette => 0,
        FeatureClass::Decal => 1,
        FeatureClass::Hatch { level } => 2 + u64::from(level),
    }
}

fn fold(cell: (i64, i64, i64)) -> u64 {
    (cell.0 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (cell.1 as u64).rotate_left(21)
        ^ (cell.2 as u64).rotate_left(42)
}

/// The identity of one curve. Empty curves collapse to one id, which
/// costs nothing: they carry no points to lay out.
#[must_use]
pub fn curve_id(curve: &Curve3) -> CurveId {
    let (Some(first), Some(last)) = (curve.points.first(), curve.points.last()) else {
        return CurveId(hash64(curve.seed ^ class_code(curve.class)));
    };
    let (a, b) = (weld::cell(first.pos), weld::cell(last.pos));
    let (near, far) = if a <= b {
        (a, b)
    } else {
        (b, a)
    };

    CurveId(hash64(curve.seed ^ hash64(class_code(curve.class) ^ fold(near)) ^ fold(far).rotate_left(17)))
}

/// Where one curve's points sit in the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub id: CurveId,
    /// Index of the curve in the drawing the layout was taken of.
    pub curve: u32,
    /// Flat texel index of the curve's first point.
    pub start: u32,
    pub len: u32,
}

/// Why a drawing could not be laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// The drawing wants more texels than the field has. Reported
    /// rather than truncated: a silently dropped tail of the drawing is
    /// a missing outline, not a smaller one.
    OverCapacity { needed: usize, capacity: usize },
    /// One curve is longer than one bounded reduction admits.
    /// Named, so the answer is "this curve" rather than "somewhere".
    CurveTooLong { id: CurveId, curve: u32, points: usize },
}

/// The drawing's curves placed in the field, in field order.
#[derive(Clone, Debug)]
pub struct Layout {
    spans: Vec<Span>,
    by_curve: Vec<u32>,
    /// How many leading spans belong to the resident region.
    resident: usize,
    occupied: usize,
}

impl Layout {
    /// The spans in field order — ascending `start`: the resident
    /// region first, then the volatile one, each ordered by
    /// [`CurveId`].
    #[must_use]
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// The resident region's spans — the texels that stay put while the
    /// eye moves, and so the ones [`RESIDENT`]'s buffer is packed from.
    #[must_use]
    pub fn resident(&self) -> &[Span] {
        &self.spans[..self.resident]
    }

    /// The volatile region's spans, packed into [`VOLATILE`]'s buffer
    /// every re-split.
    #[must_use]
    pub fn volatile(&self) -> &[Span] {
        &self.spans[self.resident..]
    }

    /// Where the drawing's `curve`th curve landed, indexed over the two
    /// halves concatenated — [`Drawing::curves`]' own order.
    #[must_use]
    pub fn span_of(&self, curve: usize) -> Option<&Span> {
        self.spans.get(*self.by_curve.get(curve)? as usize)
    }

    /// Texels the drawing occupies, gaps included — the number the
    /// capacity is spent against.
    #[must_use]
    pub fn occupied(&self) -> usize {
        self.occupied
    }

    /// Points the drawing carries, which is also how many texel
    /// triangles the two [`point_vertices`] calls pack between them.
    #[must_use]
    pub fn points(&self) -> usize {
        points_of(&self.spans)
    }
}

/// Points a run of spans carries.
fn points_of(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.len as usize).sum()
}

/// Place a drawing's curves in a field of `field` texels.
///
/// The resident half is placed first and the volatile half after it, so
/// a resident curve's span depends on nothing but the resident half
/// itself. That is what makes its packed points *resident*: the same
/// curves lay out at the same texels every frame, whatever the eye did
/// to the volatile half, so the buffer the GPU already holds is still
/// the right one. It is also what the deferred persistent-allocator
/// note on iamacoffeepot/aether#4428 was asking for — the two regions
/// are the whole allocator, because the resident half only ever changes
/// wholesale, with the subject.
///
/// Within each half the order is by identity rather than by arrival, so
/// the same drawing lays out the same way whatever order the extractor
/// enumerated it in. Each span is preceded by one empty texel,
/// including the first — that leading gap is what gives the first
/// curve's first point a barrier behind it, and the volatile region
/// inherits the barrier the resident region's last span left.
pub fn layout(drawing: Drawing<'_>, field: (u32, u32)) -> Result<Layout, LayoutError> {
    let capacity = field.0 as usize * field.1 as usize;

    let mut spans: Vec<Span> = Vec::with_capacity(drawing.len());
    let mut by_curve = vec![0u32; drawing.len()];
    let mut cursor = 1usize;
    for half in [drawing.resident, drawing.volatile] {
        let base = spans.len() as u32;
        let mut order: Vec<(CurveId, u32)> =
            half.iter().enumerate().map(|(at, curve)| (curve_id(curve), base + at as u32)).collect();
        order.sort_unstable();

        for (id, curve) in order {
            let points = half[(curve - base) as usize].points.len();
            if points > MAX_CURVE_POINTS {
                return Err(LayoutError::CurveTooLong { id, curve, points });
            }
            by_curve[curve as usize] = spans.len() as u32;
            spans.push(Span { id, curve, start: cursor as u32, len: points as u32 });
            cursor += points + 1;
        }
    }
    if cursor > capacity {
        return Err(LayoutError::OverCapacity { needed: cursor, capacity });
    }

    Ok(Layout { spans, by_curve, resident: drawing.resident.len(), occupied: cursor })
}

/// The subject's vertex buffer for the prepass: the rest position and
/// the bone binding that poses it.
///
/// Packed once per subject rather than once per pose. The whole point of
/// the binding riding here is that this buffer never travels again: a
/// pose moves every vertex, and re-uploading the mesh per frame was the
/// prepass' share of what iamacoffeepot/aether#4462 deleted.
///
/// Without a rig every vertex packs an empty share row, which the vertex
/// stage reads as "this position is already the answer".
#[must_use]
pub fn subject_vertices(mesh: &Mesh, skin: Option<&Skin>) -> Vec<u8> {
    let mut packed = Vec::with_capacity(mesh.positions.len() * SUBJECT_VERTEX_BYTES);
    for (vertex, position) in mesh.positions.iter().enumerate() {
        packed.extend(position.to_array().into_iter().flat_map(f32::to_le_bytes));
        let (joints, shares) = skin.map_or(([0; INFLUENCES], [0.0; INFLUENCES]), |skin| skin.influences(vertex));
        packed.extend(joints);
        packed.extend(shares.map(unorm));
    }

    packed
}

/// One share as the `Unorm8x4` lane carries it.
fn unorm(share: f32) -> u8 {
    (share.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The class code a point's `stroke` lane carries: the hatch family's
/// level, or negative where the class grazes the eye by definition.
///
/// One lane for two questions because they are the same partition.
/// `visibility::drawn` exempts the silhouette and the decal from the
/// facing test, and [`crate::extract::tone_gate`] hands those two classes
/// straight back ungated — so what the shader needs to know is which
/// side of that line the curve is on, and if it is on the hatch side,
/// which of the three thresholds applies.
fn stroke_class(class: FeatureClass) -> f32 {
    match class {
        FeatureClass::Silhouette | FeatureClass::Decal => -1.0,
        FeatureClass::Hatch { level } => f32::from(level),
    }
}

/// The subject's index buffer: the face list as little-endian `u32`
/// triangle-list indices.
#[must_use]
pub fn subject_indices(mesh: &Mesh) -> Vec<u8> {
    mesh.faces.iter().flatten().flat_map(|corner| corner.to_le_bytes()).collect()
}

/// One region's vertex buffer, packed for [`points_slot`]: three
/// identical vertices per point, differing only in the corner bits of
/// `slot`.
///
/// `spans` selects the region — [`Layout::resident`] or
/// [`Layout::volatile`] — and the drawing addresses the curves, since a
/// [`Span`] names its own by the index
/// [`Drawing::curves`](crate::feature::Drawing::curves) gives it.
///
/// This is the [`RESIDENT`] region's layout, and it is the region rather
/// than the rig that chooses it: a geometry slot is declared once when
/// the program registers and cannot ask whether *this* subject carries a
/// rig. So `bound` decides only whether the corners are real. Without a
/// rig the point stands for itself, in an anchorage whose share row is
/// empty — which the vertex stage reads as "this position is already the
/// answer" and poses no further.
///
/// The resident half is the *rest* surface curves, so packing them
/// against their anchorages is what lets the vertex stage pose them from
/// the uniform blob — and what lets them stay resident through a pose as
/// they already stayed resident through an orbit.
/// [`posed_point_vertices`] is the volatile half's, which is re-solved
/// on the CPU every frame against whatever pose is running.
///
/// A region that laid out to nothing still packs one stand-in point, for
/// the reason [`super::stroke::ribbon_geometry`] packs one stand-in
/// vertex: a dispatch supplies one id per declared geometry slot or it
/// warn-drops whole, so an empty region has to neutralize through the
/// content rather than by restructuring the graph. The stand-in is
/// three copies of one corner — a triangle of zero area, which
/// rasterizes nothing wherever it lands. A stand-in placed *on* the
/// field would not do: every texel of the field is either a point or
/// the barrier that ends a curve, so writing a verdict into a free one
/// would answer for a barrier that has to read as hidden.
#[must_use]
pub fn point_vertices(drawing: Drawing<'_>, spans: &[Span], bound: Option<Bound<'_>>) -> Vec<u8> {
    points(drawing, spans, bound, true)
}

/// The [`VOLATILE`] region's own layout: where each point already is.
///
/// See [`posed_points_slot`] for why the two regions divide.
#[must_use]
pub fn posed_point_vertices(drawing: Drawing<'_>, spans: &[Span]) -> Vec<u8> {
    points(drawing, spans, None, false)
}

fn points(drawing: Drawing<'_>, spans: &[Span], bound: Option<Bound<'_>>, anchored_layout: bool) -> Vec<u8> {
    // Assembled once per point on the stack and copied in three times
    // with only the corner bits of `slot` patched between them, which is
    // the only thing three vertices of one point disagree about. Written
    // lane by lane instead it is a bounds check per byte over megabytes
    // of drawing, and that showed up as whole milliseconds of the frame.
    let stride = if anchored_layout {
        POINT_VERTEX_BYTES
    } else {
        POSED_POINT_VERTEX_BYTES
    };
    let mut packed: Vec<u8> = Vec::with_capacity(points_of(spans).max(1) * 3 * stride);
    let mut written = false;
    for span in spans {
        let Some(curve) = drawing.curve(span.curve as usize) else {
            continue;
        };
        let class = stroke_class(curve.class);
        let last = curve.points.len().saturating_sub(1);
        for (at, point) in curve.points.iter().enumerate() {
            // World span to the next point, which the shader divides by
            // the point's own distance to the eye — `ribbon`'s angular
            // measure, arrived at without re-uploading when the camera
            // turns. Zero at the last point, so the empty texel after
            // it sits at zero arc and reads as the curve's end.
            //
            // Measured at rest for a bound curve, and left there. The
            // rig is a composition of rotations about pivots, so it
            // moves a chord between two neighbouring points by the
            // weight gradient across one triangle edge — and the arc
            // this feeds is a taper's ramp, not a position.
            let step = curve.points.get(at + 1).map_or(0.0, |next| (next.pos - point.pos).length());
            let anchored = anchored_layout.then(|| {
                bound
                    .zip(point.anchorage)
                    .map_or_else(|| Anchored::posed(point.probe, point.normal), |(bound, at)| bound.anchored(at))
            });
            let tail = [at as f32, (last - at) as f32, step, class];
            let vertex = pack_point(anchored.as_ref(), point.probe, point.normal, span.start + at as u32, tail);
            vertex.write(&mut packed);
            written = true;
        }
    }
    if !written {
        // A triangle of zero area, which rasterizes nothing wherever it
        // lands. It has to carry the region's own layout, since which of
        // the two the slot was declared with is what the vertex fetch
        // reads it as.
        let inert = anchored_layout.then(|| Anchored::posed(Vec3::ZERO, Vec3::new(0.0, 0.0, 1.0)));
        pack_point(inert.as_ref(), Vec3::ZERO, Vec3::new(0.0, 0.0, 1.0), 0, [0.0; 4]).write_flat(&mut packed);
    }

    packed
}

/// One point's vertex bytes in whichever of the two layouts its region
/// declared, with `slot` still holding the bare texel index.
struct Point {
    bytes: [u8; POINT_VERTEX_BYTES],
    len: usize,
    /// Where the `slot` lane starts, which is the one lane the three
    /// corners disagree about.
    slot: usize,
}

impl Point {
    /// The point's three vertices, each with its own corner index folded
    /// into the low bits of `slot`.
    fn write(&self, packed: &mut Vec<u8>) {
        let bare = f32::from_le_bytes(self.bytes[self.slot..self.slot + 4].try_into().expect("four bytes"));
        for corner in 0..3u32 {
            packed.extend_from_slice(&self.bytes[..self.slot]);
            packed.extend_from_slice(&(bare + corner as f32).to_le_bytes());
            packed.extend_from_slice(&self.bytes[self.slot + 4..self.len]);
        }
    }

    /// The same three vertices as one corner — the stand-in's zero-area
    /// triangle, which covers no sample wherever it lands.
    fn write_flat(&self, packed: &mut Vec<u8>) {
        for _ in 0..3 {
            packed.extend_from_slice(&self.bytes[..self.len]);
        }
    }
}

fn pack_point(anchored: Option<&Anchored>, probe: Vec3, normal: Vec3, texel: u32, tail: [f32; 4]) -> Point {
    let mut bytes = [0u8; POINT_VERTEX_BYTES];
    let mut at = 0usize;
    let mut lay = |written: &[u8]| {
        bytes[at..at + written.len()].copy_from_slice(written);
        at += written.len();
    };
    match anchored {
        Some(anchored) => {
            for corner in 0..2usize {
                for value in
                    anchored.positions[corner].to_array().into_iter().chain(anchored.normals[corner].to_array())
                {
                    lay(&value.to_le_bytes());
                }
                lay(&anchored.joints[corner]);
                lay(&anchored.shares[corner].map(unorm));
            }
        }
        None => {
            for value in probe.to_array().into_iter().chain(normal.to_array()) {
                lay(&value.to_le_bytes());
            }
        }
    }
    let slot = if anchored.is_some() {
        POINT_SLOT
    } else {
        POSED_POINT_SLOT
    };
    lay(&((texel * 4) as f32).to_le_bytes());
    for value in tail {
        lay(&value.to_le_bytes());
    }
    if let Some(anchored) = anchored {
        lay(&anchored.between.to_le_bytes());
    }
    let len = at;

    Point { bytes, len, slot }
}

/// Where the `slot` lane starts in each of the two layouts: after the
/// two corner bindings, or after the point's own position and normal.
const POINT_SLOT: usize = 2 * (12 + 12 + 4 + 4);
const POSED_POINT_SLOT: usize = 12 + 12;

/// One region's index buffer: sequential little-endian `u32`
/// triangle-list indices over the same `spans` [`point_vertices`] was
/// handed. Nothing is shared — a point's three vertices differ in their
/// corner bits, and no two points share a vertex.
#[must_use]
pub fn point_indices(spans: &[Span]) -> Vec<u8> {
    let count = u32::try_from(points_of(spans).max(1) * 3).unwrap_or(u32::MAX);

    (0..count).flat_map(u32::to_le_bytes).collect()
}

/// The drawing's curve blocks, one texel-sized triangle each, carrying
/// the eye-independent bounds of the reduction the GPU runs for it.
///
/// `slot` packs the same way a point's does — the flat field index
/// shifted up two bits with the corner in the low pair — because the
/// two rasterize through the same placement.
#[must_use]
pub fn curves_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32 },
            VertexAttribute { location: 1, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 2, format: VertexFormat::Float32x2 },
        ],
    }
}

/// Bytes one curve-block vertex occupies: the packed slot, curve start,
/// block length, block count, curve length and minimum angular length.
pub const CURVE_VERTEX_BYTES: usize = 4 + 12 + 8;

/// The whole drawing's per-curve buffer, packed for [`curves_slot`]:
/// one triangle per [`CURVE_BLOCK_POINTS`] points, placed over the
/// curve's first texels and carrying the curve-wide length policy.
///
/// Not divided by volatility, unlike the points. One record per at most
/// [`CURVE_BLOCK_POINTS`] points is still far smaller than either point
/// buffer, and the block list changes whenever the volatile drawing
/// changes.
/// It does not depend on the eye: the camera reaches the point-depth and
/// angular-step planes through the uniform blob. Sparse block draws
/// reduce those in parallel, then the first block folds their totals for
/// reference depth and coverage without a CPU point walk.
/// The eye argument remains in this packer's contract because its
/// callers stage curve metadata at the same per-view boundary as the
/// volatile drawing; it is deliberately not read.
///
/// A drawing that laid out to nothing still packs one stand-in, for the
/// reason [`point_vertices`] does, and by the same means: three copies
/// of one corner, which is a triangle of zero area. An empty curve gets
/// one real zero-length block so its reference still resolves to
/// [`ribbon::NOT_DRAWN`].
#[must_use]
pub fn curve_vertices(drawing: Drawing<'_>, layout: &Layout, _eye: Vec3) -> Vec<u8> {
    let mut blocks: Vec<[f32; 6]> = Vec::new();
    for span in layout.spans() {
        let Some(curve) = drawing.curve(span.curve as usize) else {
            continue;
        };
        let block_count = (span.len as usize).div_ceil(CURVE_BLOCK_POINTS).max(1);
        let length_floor = ribbon::minimum_angular_length(curve);
        blocks.reserve(block_count);
        for block in 0..block_count {
            let consumed = block * CURVE_BLOCK_POINTS;
            let block_len = (span.len as usize).saturating_sub(consumed).min(CURVE_BLOCK_POINTS);
            blocks.push([
                (span.start as usize + block) as f32,
                span.start as f32,
                block_len as f32,
                block_count as f32,
                span.len as f32,
                length_floor,
            ]);
        }
    }
    let stand_in = blocks.is_empty();
    if stand_in {
        blocks.push([0.0; 6]);
    }

    let mut packed: Vec<u8> = Vec::with_capacity(blocks.len() * 3 * CURVE_VERTEX_BYTES);
    for block in blocks {
        for corner in 0..3u32 {
            let slot = block[0] * 4.0
                + if stand_in {
                    0.0
                } else {
                    corner as f32
                };
            packed.extend(
                [slot, block[1], block[2], block[3], block[4], block[5]].into_iter().flat_map(f32::to_le_bytes),
            );
        }
    }

    packed
}

/// The curve-block index buffer, matching [`curve_vertices`]: sequential
/// little-endian `u32` triangle-list indices, nothing shared.
#[must_use]
pub fn curve_indices(layout: &Layout) -> Vec<u8> {
    let blocks =
        layout.spans().iter().map(|span| (span.len as usize).div_ceil(CURVE_BLOCK_POINTS).max(1)).sum::<usize>();
    let count = u32::try_from(blocks.max(1) * 3).unwrap_or(u32::MAX);

    (0..count).flat_map(u32::to_le_bytes).collect()
}

fn draw(
    vertex_entry_point: &str,
    entry_point: &str,
    geometry: u32,
    depth: Option<u32>,
    inputs: Vec<InputSlot>,
    output: OutputSlot,
    load: PassLoad,
) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Draw(DrawPass { vertex_entry_point: vertex_entry_point.to_owned(), geometry, depth, load }),
        entry_point: entry_point.to_owned(),
        inputs,
        output,
        uniform_offset: SightUniforms::window(0),
        uniform_length: SightUniforms::BYTES,
        repeat: None,
    }
}

/// One fragment stage run over both point buffers: the resident half
/// clears the target and the volatile half loads what it left.
///
/// The two halves own disjoint texels — [`layout`] gives them disjoint
/// spans — so which of them a texel's value came from is decided by the
/// layout rather than by the draw order, and the pair together writes
/// exactly what one pass over the whole drawing wrote.
fn over_points(entry_point: &str, inputs: Vec<InputSlot>, output: OutputSlot) -> [ProgramPass; 2] {
    [
        draw("vs_point", entry_point, RESIDENT, None, inputs.clone(), output, PassLoad::Clear),
        draw("vs_point_posed", entry_point, VOLATILE, None, inputs, output, PassLoad::Load),
    ]
}

fn scan(entry_point: &str, step: u32, inputs: Vec<InputSlot>, output: OutputSlot) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: entry_point.to_owned(),
        inputs,
        output,
        uniform_offset: SightUniforms::window(step),
        uniform_length: SightUniforms::BYTES,
        repeat: None,
    }
}

/// The whole field as one register graph.
///
/// Static by construction: the structure depends only on the reach scan
/// depth, so it is the same graph for every drawing at every canvas
/// size. The reach scan ping-pongs between pairs of transients because a
/// pass may not read the slot it writes, and walks the arc pair alongside
/// — the arc a doubling has to pay is itself a doubling, since arc is not
/// the index. Curve-wide quantities are sparse draw reductions instead
/// of full-field scan chains.
#[must_use]
pub fn program() -> ProgramRegister {
    let transient = |index: u32| OutputSlot::Transient { index };
    let read = |index: u32| InputSlot::Transient { index };
    let binding = |index: u32| OutputSlot::Binding { index };
    let bound = |index: u32| InputSlot::Binding { index };

    let mut passes =
        vec![draw("vs_subject", "fs_depth", SUBJECT, Some(0), Vec::new(), transient(DEPTH), PassLoad::Clear)];
    passes.extend(over_points("fs_seen", vec![read(DEPTH)], binding(SEEN)));
    passes.extend(over_points("fs_step", Vec::new(), transient(STEP)));
    passes.extend(over_points("fs_head", Vec::new(), transient(HEAD)));
    passes.extend(over_points("fs_point_depth", Vec::new(), transient(POINT_DEPTH)));
    // The block triangles occupy the first texels of each curve's own
    // span. Their fragments reduce bounded point runs in parallel; then
    // only the first block triangle survives `vs_curve` to fold the
    // short list of block totals for the curve.
    passes.push(draw(
        "vs_curve_block",
        "fs_block_depth",
        CURVES,
        None,
        vec![read(POINT_DEPTH)],
        transient(BLOCK_DEPTH),
        PassLoad::Clear,
    ));
    passes.push(draw(
        "vs_curve_block",
        "fs_block_arc",
        CURVES,
        None,
        vec![read(STEP)],
        transient(BLOCK_ARC),
        PassLoad::Clear,
    ));
    passes.push(draw(
        "vs_curve",
        "fs_curve_reference",
        CURVES,
        None,
        vec![read(BLOCK_DEPTH), read(BLOCK_ARC)],
        binding(REFERENCE),
        PassLoad::Clear,
    ));
    passes.push(draw(
        "vs_curve_block",
        "fs_block_coverage",
        CURVES,
        None,
        vec![bound(SEEN)],
        transient(BLOCK_COVERAGE),
        PassLoad::Clear,
    ));
    passes.push(draw(
        "vs_curve",
        "fs_curve_coverage",
        CURVES,
        None,
        vec![read(BLOCK_COVERAGE)],
        transient(CURVE_COVERAGE),
        PassLoad::Clear,
    ));
    passes.push(scan("fs_cover_gather", 0, vec![read(CURVE_COVERAGE), read(HEAD)], binding(COVERAGE)));
    passes.extend([
        scan("fs_reach_seed", 0, vec![bound(SEEN)], transient(HEAD_SPREAD[0])),
        scan("fs_reach_seed", 0, vec![bound(SEEN)], transient(TAIL_SPREAD[0])),
    ]);

    // The reach scan, walked separately in each direction. Doubling `k`
    // relaxes each texel against the one `2^k` away and pays the arc
    // between them, which is the arc chain at its own `k` — so the
    // three advance in step, the arc's next doubling laid down right
    // after the pair of reach passes that consumed this one.
    //
    // Two directions rather than one symmetric relaxation because the
    // taper needs the run's arc, not only the nearest barrier: a `min`
    // taken inside the scan cannot be un-taken, and
    // `head + tail` is the run. The arc chain is shared, so the second
    // direction costs the reach passes alone.
    let mut head = 0usize;
    let mut tail = 0usize;
    let mut arc = STEP;
    for step in 0..REACH_STEPS {
        passes.push(scan(
            "fs_head_step",
            step,
            vec![read(HEAD_SPREAD[head]), read(arc)],
            transient(HEAD_SPREAD[1 - head]),
        ));
        head = 1 - head;
        passes.push(scan(
            "fs_tail_step",
            step,
            vec![read(TAIL_SPREAD[tail]), read(arc)],
            transient(TAIL_SPREAD[1 - tail]),
        ));
        tail = 1 - tail;
        if step + 1 < REACH_STEPS {
            let next = ARC[(step % 2) as usize];
            passes.push(scan("fs_arc_step", step, vec![read(arc)], transient(next)));
            arc = next;
        }
    }
    let (headed, tailed) = (HEAD_SPREAD[head], TAIL_SPREAD[tail]);

    passes.push(scan("fs_total_out", 0, vec![read(headed), read(tailed)], binding(TOTAL)));
    // Last, because a program's final pass writes the binding whose
    // texture is the reference extent every other slot resolves from.
    passes.push(scan("fs_reach_out", 0, vec![read(headed), read(tailed)], binding(REACH)));

    ProgramRegister {
        // The two preludes after this module's own source: the vertex
        // stage poses the drawing from the bone table, and `fs_seen`
        // gates hatching against the normal that posing produced.
        wgsl: format!("{SIGHT_WGSL}\n{SKIN_WGSL}\n{TONE_WGSL}"),
        bindings: vec![plane_slot(); PLANE_COUNT],
        transients: vec![plane_slot(); TRANSIENT_COUNT],
        geometries: vec![subject_slot(), points_slot(), posed_points_slot(), curves_slot()],
        depth_transients: vec![SlotExtent::Full],
        passes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deform::{bone_uniform, npy};
    use crate::feature::{Pen, SurfacePoint};
    use aether_render::vertex_stride_bytes;

    /// A drawing whose halves are not the point of the test: everything
    /// volatile, which is what the layout did before it had two regions.
    fn whole(curves: &[Curve3]) -> Drawing<'_> {
        Drawing { resident: &[], volatile: curves }
    }

    fn curve(seed: u64, at: f32, points: usize) -> Curve3 {
        Curve3 {
            points: (0..points)
                .map(|i| SurfacePoint::on_surface(Vec3::new(at + i as f32 * 0.01, 0.0, 0.0), Vec3::new(0.0, 0.0, 1.0)))
                .collect(),
            class: FeatureClass::Silhouette,
            pen: Pen::Ink,
            seed,
            authored: false,
        }
    }

    /// A strip and a rig binding its four corners — the smallest thing
    /// the bound packer will take.
    fn bound_fixture() -> (Mesh, Skin) {
        const OBJ: &[u8] = b"v -1 0 0\nv 1 0 0\nv -1 1 0\nv 1 1 0\nf 1 2 3\nf 2 4 3\n";
        let mesh = Mesh::from_obj_bytes(OBJ, 0).expect("a strip is a mesh");
        let weights = npy(&[1.0, 0.0, 0.7, 0.3, 0.4, 0.6, 0.0, 1.0], (4, 2));
        let skin = Skin::parse(&weights, "bones chest head\npivot head 0.0 0.0 0.0\n", 4).expect("a four-vertex rig");

        (mesh, skin)
    }

    /// A level set across that strip, so every point carries the
    /// anchorage the bound packer reads.
    fn anchored_curves(mesh: &Mesh) -> Vec<Curve3> {
        let heights: Vec<f32> = mesh.positions.iter().map(|at| at.y).collect();
        let template = Curve3 {
            points: Vec::new(),
            class: FeatureClass::Hatch { level: 0 },
            pen: Pen::Pale,
            seed: 0,
            authored: false,
        };
        let crossings = mesh
            .level_set(&heights, &[], 0.5)
            .into_iter()
            .map(|[a, b]| [SurfacePoint::anchored(&a), SurfacePoint::anchored(&b)])
            .collect();

        weld::curves(crossings, &template)
    }

    /// Tripwire: the packers and the declared layouts must agree on
    /// their strides. They are two independent statements of one byte
    /// arrangement — the layout builds the vertex buffer layout, the
    /// packer writes the bytes — and a disagreement is not a compile
    /// error but a silently reinterpreted drawing, every attribute
    /// sliding one lane per vertex.
    #[test]
    fn the_packed_vertices_match_the_declared_strides() {
        assert_eq!(vertex_stride_bytes(&subject_slot().layout), SUBJECT_VERTEX_BYTES, "subject stride");
        assert_eq!(vertex_stride_bytes(&points_slot().layout), POINT_VERTEX_BYTES, "point stride");
        assert_eq!(vertex_stride_bytes(&posed_points_slot().layout), POSED_POINT_VERTEX_BYTES, "posed point stride");
        assert_eq!(vertex_stride_bytes(&curves_slot().layout), CURVE_VERTEX_BYTES, "curve stride");

        let curves = [curve(1, 0.0, 4), curve(2, 1.0, 7)];
        let placed = layout(whole(&curves), (64, 64)).expect("fits");
        assert_eq!(
            posed_point_vertices(whole(&curves), placed.spans()).len(),
            11 * 3 * POSED_POINT_VERTEX_BYTES,
            "packed length"
        );
        assert_eq!(point_indices(placed.spans()).len(), 11 * 3 * 4, "index length");

        // And the bound region's, which is a second byte arrangement
        // over the same points: a region packed at one stride into a
        // slot declared at the other reads every lane one place along.
        let (mesh, skin) = bound_fixture();
        let anchored = anchored_curves(&mesh);
        let bound = layout(whole(&anchored), (64, 64)).expect("fits");
        assert_eq!(
            point_vertices(whole(&anchored), bound.spans(), Some(Bound { rest: &mesh, skin: &skin })).len(),
            bound.points() * 3 * POINT_VERTEX_BYTES,
            "bound packed length",
        );

        assert_eq!(
            curve_vertices(whole(&curves), &placed, Vec3::ZERO).len(),
            2 * 3 * CURVE_VERTEX_BYTES,
            "curve length"
        );
        assert_eq!(curve_indices(&placed).len(), 2 * 3 * 4, "curve index length");

        let long = [curve(3, 2.0, CURVE_BLOCK_POINTS * 2 + 1)];
        let blocked = layout(whole(&long), (64, 64)).expect("fits");
        assert_eq!(
            curve_vertices(whole(&long), &blocked, Vec3::ZERO).len(),
            3 * 3 * CURVE_VERTEX_BYTES,
            "three reduction blocks"
        );
        assert_eq!(curve_indices(&blocked).len(), 3 * 3 * 4, "three block triangles");
    }

    /// Tripwire: the curve reduction receives the exact length-floor
    /// policy and span length it needs to decide the reference sign.
    ///
    /// The floor used to remove a curve's ribbon vertices outright.
    /// Now the vertices are packed whatever the eye thinks and the GPU
    /// reduction negates the reference instead. A missing floor puts
    /// every speck it exists to reject back on the paper at full weight,
    /// with nothing having errored. The exemption is half the rule and
    /// is checked with it: an authored mark carries a zero floor.
    #[test]
    fn the_curve_reduction_carries_length_and_floor_policy() {
        let speck = |authored: bool| {
            let mut one = curve(1, 0.0, 3);
            one.points.iter_mut().enumerate().for_each(|(at, point)| {
                point.pos = Vec3::new(at as f32 * 1.0e-4, 0.0, 0.0);
                point.probe = point.pos;
            });
            one.authored = authored;
            let expected = ribbon::minimum_angular_length(&one);
            let drawn = [one];
            let placed = layout(whole(&drawn), (64, 64)).expect("fits");
            let packed = curve_vertices(whole(&drawn), &placed, Vec3::ZERO);

            (
                f32::from_le_bytes(packed[8..12].try_into().expect("the block length lane")),
                f32::from_le_bytes(packed[12..16].try_into().expect("the block count lane")),
                f32::from_le_bytes(packed[16..20].try_into().expect("the curve length lane")),
                f32::from_le_bytes(packed[20..24].try_into().expect("the floor lane")),
                expected,
            )
        };

        let extracted = speck(false);
        let authored = speck(true);
        assert_eq!((extracted.0, extracted.1, extracted.2), (3.0, 1.0, 3.0), "block and curve lengths");
        assert_eq!(extracted.3, extracted.4, "extracted floor");
        assert!(extracted.3 > 0.0, "an extracted speck has a floor");
        assert_eq!(authored, (3.0, 1.0, 3.0, 0.0, 0.0), "an authored mark is exempt");
    }

    /// Tripwire: an empty region still packs one point, and that point
    /// draws nothing.
    ///
    /// A dispatch supplies one id per declared geometry slot or it
    /// warn-drops whole, so a region that laid out to nothing must still
    /// produce a geometry the dispatch can name — otherwise the field
    /// silently stops updating rather than updating to empty. The
    /// stand-in has to be inert as well as present: every texel of the
    /// field is a point or the barrier that ends a curve, so a stand-in
    /// that rasterized anywhere would answer for one of them. Three
    /// copies of one corner is a triangle of zero area, which covers no
    /// sample wherever it lands.
    #[test]
    fn an_empty_region_packs_one_point_that_draws_nothing() {
        let placed = layout(whole(&[]), (64, 64)).expect("fits");
        let packed = posed_point_vertices(whole(&[]), placed.spans());

        assert_eq!(packed.len(), 3 * POSED_POINT_VERTEX_BYTES);
        assert_eq!(point_indices(placed.spans()).len(), 3 * 4);
        let (first, rest) = packed.split_at(POSED_POINT_VERTEX_BYTES);
        assert_eq!(rest, [first, first].concat(), "the stand-in's three vertices are one corner");
    }

    /// Tripwire: a resident curve keeps its texels when the volatile
    /// half changes underneath it.
    ///
    /// This is the whole basis of the residency (#4435). The resident
    /// buffer is uploaded once and then left alone, so the GPU keeps
    /// reading whatever spans it was packed against; the day a volatile
    /// curve appearing or growing shifts a resident span, every resident
    /// point silently addresses a texel belonging to some other stroke
    /// and the drawing reads its occlusion off the wrong curves.
    #[test]
    fn a_resident_span_survives_the_volatile_half_changing() {
        let resident = [curve(1, 0.0, 4), curve(2, 1.0, 7)];
        let thin = [curve(3, 2.0, 3)];
        let thick = [curve(3, 2.0, 9), curve(4, 3.0, 5)];

        let before = layout(Drawing { resident: &resident, volatile: &thin }, (64, 64)).expect("fits");
        let after = layout(Drawing { resident: &resident, volatile: &thick }, (64, 64)).expect("fits");

        assert_eq!(before.resident(), after.resident(), "the resident region");
        assert_eq!(before.volatile().len(), 1, "the volatile region is the volatile half");
        assert_eq!(after.volatile().len(), 2);
    }

    /// Tripwire: every curve is flanked by an empty texel, and no two
    /// curves overlap.
    ///
    /// The gap is what the reach scan reads as a curve end — without it
    /// a scan walking off one curve's last point lands on the next
    /// curve's first and the taper leaks between two unrelated strokes,
    /// which looks like a stroke that simply never tapers.
    #[test]
    fn every_span_is_flanked_by_an_empty_texel() {
        let curves = [curve(1, 0.0, 4), curve(2, 1.0, 7), curve(3, 2.0, 2)];
        let placed = layout(whole(&curves), (64, 64)).expect("fits");

        let mut previous_end = 0;
        for span in placed.spans() {
            assert!(span.start > previous_end, "a gap before {span:?}, after texel {previous_end}");
            previous_end = span.start + span.len;
        }
        assert_eq!(placed.occupied(), 13 + 3 + 1, "points, one gap each, and the leading one");
    }

    /// Tripwire: a curve's span follows its identity, not the order the
    /// extractor happened to hand it over in.
    ///
    /// The field is addressed by span, so the day anything persists
    /// across a re-split — a partial re-upload, a temporal reuse — it
    /// keys on this. An ordinal layout passes every other test here and
    /// silently re-seats the whole drawing the moment one curve is
    /// added.
    #[test]
    fn a_span_follows_the_curve_not_its_arrival_order() {
        let curves = [curve(1, 0.0, 4), curve(2, 1.0, 7), curve(3, 2.0, 2)];
        let shuffled = [curves[2].clone(), curves[0].clone(), curves[1].clone()];

        let placed = layout(whole(&curves), (64, 64)).expect("fits");
        let reordered = layout(whole(&shuffled), (64, 64)).expect("fits");

        for (at, was) in [(0usize, 1usize), (1, 2), (2, 0)] {
            let before = placed.span_of(at).expect("placed");
            let after = reordered.span_of(was).expect("reordered");
            assert_eq!((before.id, before.start, before.len), (after.id, after.start, after.len), "curve {at}");
        }
    }

    /// Tripwire: a curve welded from its far end is the same curve.
    ///
    /// The weld resolves a junction by whichever partner it meets
    /// first, so the direction a stroke comes out in is not stable
    /// across re-splits. An id that reversed with the points would make
    /// every such stroke a new curve and the stability above a fiction.
    #[test]
    fn a_reversed_curve_keeps_its_identity() {
        let forward = curve(7, 0.0, 5);
        let mut backward = forward.clone();
        backward.points.reverse();

        assert_eq!(curve_id(&forward), curve_id(&backward));
    }

    /// Tripwire: the layout refuses what it cannot lay out, by name.
    ///
    /// Both refusals are the alternative to a silent truncation, and a
    /// silent truncation here is a missing outline that nothing
    /// reports.
    #[test]
    fn the_layout_refuses_rather_than_truncates() {
        let long = [curve(1, 0.0, MAX_CURVE_POINTS + 1)];
        assert!(
            matches!(layout(whole(&long), (4096, 4096)), Err(LayoutError::CurveTooLong { points, .. }) if points == MAX_CURVE_POINTS + 1),
        );

        let many = [curve(1, 0.0, 40), curve(2, 1.0, 40)];
        assert!(matches!(layout(whole(&many), (8, 8)), Err(LayoutError::OverCapacity { capacity: 64, .. })));
    }

    /// Tripwire: the uniform blob lays one window down per reach-scan
    /// doubling, each carrying its own stride.
    ///
    /// The strides are the scans' whole schedule. A blob that carried
    /// one window would compile, dispatch, and quietly run every
    /// doubling at stride one — a reach scan that resolves five points
    /// instead of thirty-one.
    #[test]
    fn every_scan_window_carries_its_own_stride() {
        let blob = SightUniforms {
            view_proj: Mat4::IDENTITY,
            eye: Vec3::new(0.0, 0.0, 5.0),
            field: (900, 1200),
            bias: 0.006,
            bones: bone_uniform(&[]),
            tone: ToneUniforms::of(&Settings::default(), false),
        }
        .encode();

        assert_eq!(blob.len(), (SightUniforms::BYTES * SightUniforms::WINDOWS) as usize, "blob length");
        for step in 0..SightUniforms::WINDOWS {
            let at = (SightUniforms::window(step) + 92) as usize;
            let stride = f32::from_le_bytes(blob[at..at + 4].try_into().expect("four bytes"));
            assert_eq!(stride, (1u32 << step) as f32, "window {step}");
        }
    }
}
