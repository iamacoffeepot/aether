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
//!   [`crate::ribbon::reference_depth`], the stroke's own average
//!   distance to the eye, with the sign carrying whether the curve is
//!   drawn at this eye at all. That is everything the eye decides about
//!   a curve rather than about one of its points, and delivering it
//!   here is what lets the ink pass solve its rails in the vertex stage
//!   from a ribbon buffer that never travels
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
//! split or truncated. What is capped is the *scan depth*, not the
//! curve: [`COVERAGE_STEPS`] doublings reduce a curve of up to
//! [`MAX_CURVE_POINTS`] points exactly, and [`layout`] refuses a longer
//! one by name rather than reducing it over a window and calling the
//! answer a coverage.
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

use crate::feature::{Curve3, Drawing, FeatureClass};
use crate::math3::hash64;
use crate::mesh::Mesh;
use crate::{ribbon, weld};

/// The field's own WGSL: the prepass, the point rasterization, and the
/// two scan chains.
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
/// drawing's points in two buffers, then one texel per curve.
///
/// The points divide by volatility rather than by class
/// ([`Drawing`]). [`RESIDENT`] carries the
/// curves that do not depend on the eye and travels once per subject;
/// [`VOLATILE`] carries the rest and travels per re-split. The two
/// occupy disjoint texels, so the passes that rasterize them write into
/// one plane with the second loading what the first laid down
/// (iamacoffeepot/aether#4435).
///
/// [`CURVES`] is neither: it carries one value per curve rather than
/// per point, so the whole drawing's worth of it is smaller than either
/// half's points and dividing it would buy nothing.
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

/// Doublings the coverage scan runs. Fifteen reduces a curve of up to
/// [`MAX_CURVE_POINTS`] points exactly, against a measured longest
/// curve of 11017 — the azimuth-90 silhouette, the worst of the four
/// framings the parity scenario walks.
pub const COVERAGE_STEPS: u32 = 15;

/// Longest curve the coverage scan reduces exactly, and so the longest
/// [`layout`] admits.
pub const MAX_CURVE_POINTS: usize = (1 << COVERAGE_STEPS) - 1;

/// Transient indices, in the order [`program`] declares them. The two
/// pairs are the scans' ping-pong: a pass may not read the slot it
/// writes, so each doubling reads one and writes the other.
const DEPTH: u32 = 0;
const STEP: u32 = 1;
const HEAD: u32 = 2;
const TAIL: u32 = 3;
const ARC: [u32; 2] = [4, 5];
const HEAD_SPREAD: [u32; 2] = [6, 7];
const SUM: [u32; 2] = [8, 9];
const TAIL_SPREAD: [u32; 2] = [10, 11];
const TRANSIENT_COUNT: usize = 12;

/// One plane's slot: full-extent `R32Float`, the format the wash's own
/// data planes already ride (ADR-0170) and the only one that carries an
/// arc without quantizing it.
#[must_use]
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// The prepass' geometry: position alone.
///
/// Nothing else is rasterized — the pass writes distance to the eye and
/// tests depth, and the point that reads it back needs no shading. A
/// pose arrives here as moved vertices or as a skinned vertex stage;
/// either way the depth image is of whatever pose the stage produced,
/// which is the property that makes this correct under animation with
/// no index to rebuild.
#[must_use]
pub fn subject_slot() -> GeometrySlotSpec {
    GeometrySlotSpec { layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }] }
}

/// Bytes one subject vertex occupies.
pub const SUBJECT_VERTEX_BYTES: usize = 12;

/// The drawing's points, one texel-sized triangle each.
///
/// `slot` is the point's flat field index shifted up two bits with its
/// corner in the low pair, because a vertex stage has no way to ask
/// which corner it is: `@builtin(vertex_index)` under an indexed draw
/// is the index *value*, which three corners of one point would share.
///
/// The two pairs after it are `ends` — points back to the curve's first
/// and on to its last — and `along` — the world span to the next point,
/// and whether the curve grazes the eye by definition.
///
/// Everything else is the point's own parameterization, and every field
/// of it is view-independent — the arc a stroke spans is carried as a
/// world length and divided by the camera in the shader. So this buffer
/// is re-uploaded when the drawing is re-extracted and not when the eye
/// moves, and a turn costs the uniform blob alone.
#[must_use]
pub fn points_slot() -> GeometrySlotSpec {
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

/// Bytes one point vertex occupies: probe, normal, slot, then the arc
/// and end pairs. Attributes pack in declaration order with no padding
/// (ADR-0171), so this is the sum of the formats' widths and
/// [`point_vertices`] writes them in exactly this order.
pub const POINT_VERTEX_BYTES: usize = 12 + 12 + 4 + 8 + 8;

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
}

impl SightUniforms {
    /// Bytes of one `SightParams` block: a `mat4x4<f32>`, a
    /// `vec3<f32>`, a `vec2<f32>` and two scalars, rounded to the
    /// struct's 16-byte alignment.
    pub const BYTES: u32 = 96;

    /// How many copies the blob carries — one per coverage doubling,
    /// which is the deeper of the two scans.
    pub const WINDOWS: u32 = COVERAGE_STEPS;

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
    /// One curve is longer than the coverage scan reduces exactly.
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

/// The subject's vertex buffer for the prepass: positions alone.
#[must_use]
pub fn subject_vertices(mesh: &Mesh) -> Vec<u8> {
    mesh.positions.iter().flat_map(|at| at.to_array()).flat_map(f32::to_le_bytes).collect()
}

/// The subject's index buffer: the face list as little-endian `u32`
/// triangle-list indices.
#[must_use]
pub fn subject_indices(mesh: &Mesh) -> Vec<u8> {
    mesh.faces.iter().flatten().flat_map(|corner| corner.to_le_bytes()).collect()
}

/// Whether a curve's class grazes the eye by definition, and so is
/// never asked to face it — `visibility::drawn`'s own test.
fn grazes(class: FeatureClass) -> bool {
    matches!(class, FeatureClass::Silhouette | FeatureClass::Decal)
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
pub fn point_vertices(drawing: Drawing<'_>, spans: &[Span]) -> Vec<u8> {
    // One point's lanes in declared order, with the flat texel index
    // still bare where `slot` goes — the corner bits go in per vertex
    // below, which is the only thing three vertices of one point
    // disagree about.
    let mut points: Vec<[f32; 11]> = Vec::with_capacity(points_of(spans));
    for span in spans {
        let Some(curve) = drawing.curve(span.curve as usize) else {
            continue;
        };
        let graze = f32::from(u8::from(grazes(curve.class)));
        let last = curve.points.len().saturating_sub(1);
        for (at, point) in curve.points.iter().enumerate() {
            // World span to the next point, which the shader divides by
            // the point's own distance to the eye — `ribbon`'s angular
            // measure, arrived at without re-uploading when the camera
            // turns. Zero at the last point, so the empty texel after
            // it sits at zero arc and reads as the curve's end.
            let step = curve.points.get(at + 1).map_or(0.0, |next| (next.pos - point.pos).length());
            let (probe, normal) = (point.probe, point.normal);
            points.push([
                probe.x,
                probe.y,
                probe.z,
                normal.x,
                normal.y,
                normal.z,
                (span.start + at as u32) as f32,
                at as f32,
                (last - at) as f32,
                step,
                graze,
            ]);
        }
    }
    let stand_in = points.is_empty();
    if stand_in {
        points.push([0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    let mut packed: Vec<u8> = Vec::with_capacity(points.len() * 3 * POINT_VERTEX_BYTES);
    for point in points {
        for corner in 0..3u32 {
            let mut vertex = point;
            vertex[6] = vertex[6] * 4.0
                + if stand_in {
                    0.0
                } else {
                    corner as f32
                };
            packed.extend(vertex.into_iter().flat_map(f32::to_le_bytes));
        }
    }

    packed
}

/// One region's index buffer: sequential little-endian `u32`
/// triangle-list indices over the same `spans` [`point_vertices`] was
/// handed. Nothing is shared — a point's three vertices differ in their
/// corner bits, and no two points share a vertex.
#[must_use]
pub fn point_indices(spans: &[Span]) -> Vec<u8> {
    let count = u32::try_from(points_of(spans).max(1) * 3).unwrap_or(u32::MAX);

    (0..count).flat_map(u32::to_le_bytes).collect()
}

/// The drawing's curves, one texel-sized triangle each, carrying the
/// one number the eye decides per curve.
///
/// `slot` packs the same way a point's does — the flat field index
/// shifted up two bits with the corner in the low pair — because the
/// two rasterize through the same placement.
#[must_use]
pub fn curves_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32 },
            VertexAttribute { location: 1, format: VertexFormat::Float32 },
        ],
    }
}

/// Bytes one curve vertex occupies: the packed slot and the reference.
pub const CURVE_VERTEX_BYTES: usize = 4 + 4;

/// The whole drawing's per-curve buffer, packed for [`curves_slot`]:
/// [`ribbon::reference_depth`] written at each curve's own first texel.
///
/// Not divided by volatility, unlike the points. The value is per curve
/// and per eye at once — a resident curve's reference depth changes
/// with the camera exactly as a volatile one's does — so there is no
/// half of it that stays put, and at one float per curve against a
/// point's eleven there is nothing to gain by splitting the walk.
///
/// A drawing that laid out to nothing still packs one stand-in, for the
/// reason [`point_vertices`] does, and by the same means: three copies
/// of one corner, which is a triangle of zero area.
#[must_use]
pub fn curve_vertices(drawing: Drawing<'_>, layout: &Layout, eye: Vec3) -> Vec<u8> {
    let mut curves: Vec<[f32; 2]> = Vec::with_capacity(layout.spans().len());
    for span in layout.spans() {
        let Some(curve) = drawing.curve(span.curve as usize) else {
            continue;
        };
        curves.push([span.start as f32, ribbon::reference_depth(curve, eye)]);
    }
    let stand_in = curves.is_empty();
    if stand_in {
        curves.push([0.0, ribbon::NOT_DRAWN]);
    }

    let mut packed: Vec<u8> = Vec::with_capacity(curves.len() * 3 * CURVE_VERTEX_BYTES);
    for curve in curves {
        for corner in 0..3u32 {
            let slot = curve[0] * 4.0
                + if stand_in {
                    0.0
                } else {
                    corner as f32
                };
            packed.extend([slot, curve[1]].into_iter().flat_map(f32::to_le_bytes));
        }
    }

    packed
}

/// The per-curve index buffer, matching [`curve_vertices`]: sequential
/// little-endian `u32` triangle-list indices, nothing shared.
#[must_use]
pub fn curve_indices(layout: &Layout) -> Vec<u8> {
    let count = u32::try_from(layout.spans().len().max(1) * 3).unwrap_or(u32::MAX);

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
        draw("vs_point", entry_point, VOLATILE, None, inputs, output, PassLoad::Load),
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
/// Static by construction: the structure depends on nothing but the two
/// scan depths, so it is the same graph for every drawing at every
/// canvas size. Both scans ping-pong between a pair of transients
/// because a pass may not read the slot it writes, and the reach scan
/// walks a second pair alongside — the arc a doubling has to pay is
/// itself a doubling, since arc is not the index.
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
    passes.extend(over_points("fs_tail", Vec::new(), transient(TAIL)));
    // One texel per curve rather than per point, and so one pass rather
    // than the pair the halves take: the value is per eye either way.
    passes.push(draw("vs_curve", "fs_reference", CURVES, None, Vec::new(), binding(REFERENCE), PassLoad::Clear));
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

    // The coverage scan: a segmented prefix sum along each curve of the
    // points that survive in a run of at least two, which is the
    // quantity `visibility::whole_or_nothing` divides.
    passes.push(scan("fs_cover_seed", 0, vec![bound(SEEN)], transient(SUM[0])));
    let mut sum = 0usize;
    for step in 0..COVERAGE_STEPS {
        passes.push(scan("fs_cover_step", step, vec![read(SUM[sum]), read(HEAD)], transient(SUM[1 - sum])));
        sum = 1 - sum;
    }
    let summed = SUM[sum];

    passes.push(scan("fs_cover_gather", 0, vec![read(summed), read(HEAD), read(TAIL)], binding(COVERAGE)));
    passes.push(scan("fs_total_out", 0, vec![read(headed), read(tailed)], binding(TOTAL)));
    // Last, because a program's final pass writes the binding whose
    // texture is the reference extent every other slot resolves from.
    passes.push(scan("fs_reach_out", 0, vec![read(headed), read(tailed)], binding(REACH)));

    ProgramRegister {
        wgsl: SIGHT_WGSL.to_owned(),
        bindings: vec![plane_slot(); PLANE_COUNT],
        transients: vec![plane_slot(); TRANSIENT_COUNT],
        geometries: vec![subject_slot(), points_slot(), points_slot(), curves_slot()],
        depth_transients: vec![SlotExtent::Full],
        passes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(vertex_stride_bytes(&curves_slot().layout), CURVE_VERTEX_BYTES, "curve stride");

        let curves = [curve(1, 0.0, 4), curve(2, 1.0, 7)];
        let placed = layout(whole(&curves), (64, 64)).expect("fits");
        assert_eq!(point_vertices(whole(&curves), placed.spans()).len(), 11 * 3 * POINT_VERTEX_BYTES, "packed length");
        assert_eq!(point_indices(placed.spans()).len(), 11 * 3 * 4, "index length");

        let eye = Vec3::new(0.0, 0.0, 4.0);
        assert_eq!(curve_vertices(whole(&curves), &placed, eye).len(), 2 * 3 * CURVE_VERTEX_BYTES, "curve length");
        assert_eq!(curve_indices(&placed).len(), 2 * 3 * 4, "curve index length");
    }

    /// Tripwire: the length floor reaches the reference plane, as its
    /// sign.
    ///
    /// The floor used to remove a curve's ribbon vertices outright.
    /// Now the vertices are packed whatever the eye thinks and the
    /// curve's reference is negated instead, so the ink pass collapses
    /// them — which means a floor that stopped reaching this plane puts
    /// every speck it exists to reject back on the paper at full
    /// weight, with nothing having errored. The exemption is half the
    /// rule and is checked with it: an authored mark of the same length
    /// still draws.
    #[test]
    fn the_length_floor_arrives_as_a_negative_reference() {
        let eye = Vec3::new(0.0, 0.0, 4.0);
        let speck = |authored: bool| {
            let mut one = curve(1, 0.0, 3);
            one.points.iter_mut().enumerate().for_each(|(at, point)| {
                point.pos = Vec3::new(at as f32 * 1.0e-4, 0.0, 0.0);
                point.probe = point.pos;
            });
            one.authored = authored;
            let drawn = [one];
            let placed = layout(whole(&drawn), (64, 64)).expect("fits");
            let packed = curve_vertices(whole(&drawn), &placed, eye);

            f32::from_le_bytes(packed[4..8].try_into().expect("the reference lane"))
        };

        assert_eq!(speck(false), ribbon::NOT_DRAWN, "an extracted speck under the floor");
        assert!(speck(true) > 0.0, "an authored mark of the same length");
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
        let packed = point_vertices(whole(&[]), placed.spans());

        assert_eq!(packed.len(), 3 * POINT_VERTEX_BYTES);
        assert_eq!(point_indices(placed.spans()).len(), 3 * 4);
        let (first, rest) = packed.split_at(POINT_VERTEX_BYTES);
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

    /// Tripwire: the uniform blob lays one window down per scan
    /// doubling, each carrying its own stride.
    ///
    /// The strides are the scans' whole schedule. A blob that carried
    /// one window would compile, dispatch, and quietly run every
    /// doubling at stride one — a reach scan that resolves five points
    /// instead of thirty-one, and a coverage scan that sums fifteen.
    #[test]
    fn every_scan_window_carries_its_own_stride() {
        let blob =
            SightUniforms { view_proj: Mat4::IDENTITY, eye: Vec3::new(0.0, 0.0, 5.0), field: (900, 1200), bias: 0.006 }
                .encode();

        assert_eq!(blob.len(), (SightUniforms::BYTES * SightUniforms::WINDOWS) as usize, "blob length");
        for step in 0..SightUniforms::WINDOWS {
            let at = (SightUniforms::window(step) + 92) as usize;
            let stride = f32::from_le_bytes(blob[at..at + 4].try_into().expect("four bytes"));
            assert_eq!(stride, (1u32 << step) as f32, "window {step}");
        }
    }
}
