//! The ink pass: the drawing rasterized on the GPU, its widths decided
//! by the visibility field rather than by a CPU run split (ADR-0172).
//!
//! # Why this is its own program
//!
//! The ink renders supersampled — twice the canvas on each edge — and
//! the program surface has no extent that multiplies. `SlotExtent` only
//! divides, and the reference every slot resolves against is the size
//! of the texture bound at the final pass' output. So the supersample
//! is expressed by *being* the reference: the ink target is created at
//! twice the canvas, that makes it the reference extent, and the
//! canvas-resolution field planes the pass reads land exactly on
//! `Divided { divisor: 2 }`. Nothing in the substrate had to grow an
//! extent arm for it.
//!
//! That also settles why the field cannot simply be another pass in
//! here. A depth attachment must match the size of the colour
//! attachment it tests, so the field's own prepass — canvas
//! resolution, shared with fifteen canvas-resolution scans — cannot
//! share a depth slot with a pass writing at twice that. The two
//! programs meet through registry textures instead, which is what the
//! field's planes already are.
//!
//! # What the vertex stage does
//!
//! Every curve is rasterized whole. The stage solves the point's rail
//! pair against the live eye — the eye-facing perpendicular, the depth
//! weighting, the hand-wobble — then reads the point's verdict, reach,
//! run arc and curve coverage out of the field, folds those into one
//! width scale, and displaces the vertex by the scaled offset. Hidden
//! points scale to zero and their segments rasterize nothing, so the
//! runs the CPU used to cut are not cut anywhere — they are what is
//! left when the hidden widths vanish.
//!
//! # What stays on the CPU
//!
//! What the eye decides per *curve*, which is one float:
//! [`ribbon::reference_depth`], delivered through the field's own
//! [`sight::REFERENCE`](super::sight::REFERENCE) plane. Everything the
//! rails need per point is a function of the curve alone — the chord
//! across a point, the wobble's world argument, the class and per-point
//! weights — so the ribbon buffer divides by volatility as the points do
//! (iamacoffeepot/aether#4440), and a resident curve's ribbon is
//! uploaded once per subject rather than once per eye.
//!
//! ADR-0172 has the rail solve staying on the CPU and travelling at
//! re-split cadence. Measured, that was 18 MB a re-split for a solve
//! whose only view-dependent input per point is the eye itself, so the
//! record's division was drawn one step too early.

use aether_math::{Mat4, Vec3};
use aether_render::{
    DrawPass, GeometrySlotSpec, InputSlot, OutputSlot, PassLoad, PassStage, ProgramPass, ProgramRegister, SlotExtent,
    SlotSpec, TextureFormat, VertexAttribute, VertexFormat,
};

use super::sight::Layout;
use super::{SKIN_WGSL, wash};
use crate::deform::{Anchored, BONE_LIMIT, Bound};
use crate::feature::{Drawing, Half};
use crate::ribbon::{self, Anchor};
use crate::style;
use crate::visibility;

/// The ink pass' own WGSL: the subject prepass and the ribbon stage.
pub const STROKE_WGSL: &str = include_str!("stroke.wgsl");

/// Dispatch-binding indices, in the order [`program`] declares them.
///
/// The five planes are the field's own bindings, supplied by naming the
/// same texture ids the sight dispatch wrote — they resolve at
/// `Divided { divisor: 2 }` against this program's doubled reference.
pub const SEEN: u32 = 0;
pub const REACH: u32 = 1;
pub const COVERAGE: u32 = 2;
pub const TOTAL: u32 = 3;
pub const REFERENCE: u32 = 4;
/// The supersampled ink sheet, and so the reference extent.
pub const INK: u32 = 5;
/// The wash's ink coverage plane: where ink stands, reduced to the wash
/// body's own extent (iamacoffeepot/aether#4451).
pub const INK_PLANE: u32 = 6;

/// How many bindings a dispatch supplies.
pub const BINDING_COUNT: usize = 7;

/// Geometry slot indices: the drawing's ribbons in two buffers, divided
/// by volatility the way the field's points are. The subject is not
/// rasterized here — see [`program`].
///
/// The two draw in this order into one raster, the resident half
/// clearing it and the volatile half loading what it left, which is
/// also the order the whole drawing composited in when it was one
/// buffer: [`Drawing`] concatenates resident first.
pub const RESIDENT: u32 = 0;
pub const VOLATILE: u32 = 1;

/// How many geometry slots a dispatch supplies.
pub const GEOMETRY_COUNT: usize = 2;

/// Edge multiple of the ink target over the canvas.
///
/// Two, against 4x MSAA on the world pass it replaces: four samples a
/// pixel either way, but resolved from a real raster rather than from
/// coverage, so a ribbon thinner than a pixel keeps its shape instead
/// of being reconstructed from a coverage fraction.
pub const SUPERSAMPLE: u32 = 2;

/// The ribbons' own raster, before the resolve turns it into something
/// a straight-alpha composite can lay down.
const RASTER: u32 = 0;

/// One field plane as this program reads it: the canvas-resolution
/// `R32Float` the sight program wrote, half this program's own edge.
#[must_use]
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Divided { divisor: SUPERSAMPLE } }
}

/// The ink sheet: full extent, and so the reference every other slot
/// resolves against. `Rgba8` because it composites over the wash by
/// alpha, and linear-sampled so the composite down to canvas resolves
/// the supersample for free.
#[must_use]
pub fn ink_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }
}

/// Raster texels per ink-coverage texel on each axis: the supersample
/// this program renders at, times the notch the wash body develops at.
///
/// Restated in the WGSL as `INK_PLANE_FOOTPRINT`, because a Rust constant
/// cannot be imported there; the two must move together.
pub const INK_PLANE_FOOTPRINT: u32 = SUPERSAMPLE * wash::BODY_DIVISOR;

/// The wash's ink coverage plane, at the extent the wash body develops
/// at — `R32Float`, the shape [`wash::program`] binds it as.
///
/// The divisor is what makes the two programs agree on one texture
/// without either knowing the other's reference extent. This program
/// resolves it against twice the canvas and the wash resolves its own
/// against the canvas, and both floor divisions land on the same texel
/// count for as long as the two layers develop at one canvas — which is
/// why [`crate::easel::wash_canvas`] is the single place either resolves
/// one.
#[must_use]
pub fn ink_plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Divided { divisor: INK_PLANE_FOOTPRINT } }
}

/// The ribbon geometry's layout: where the pen goes as an address on the
/// sculpt, the chord across the point, the two field texels the vertex
/// reads, and the ink colour.
///
/// Every lane is a function of the curve alone, which is the whole
/// point — the eye enters in the vertex stage, out of the uniform blob
/// and the reference plane, and since iamacoffeepot/aether#4462 the
/// *pose* enters the same way. So a curve that has not changed shape
/// packs the same bytes from every angle and at every pose.
///
/// The pen's position rides as the same two-corner binding
/// [`sight::points_slot`](super::sight::points_slot) carries, minus the
/// normals: a ribbon vertex needs where the point is, not which way its
/// surface faces.
///
/// The two `address` lanes are texel indices. `x` is the point's own,
/// with two bits stolen at the bottom for what a vertex stage cannot
/// otherwise ask: which of the pair's two rails it is, and whether the
/// mark was authored. `y` is the curve's first texel, where the
/// [`sight::REFERENCE`](super::sight::REFERENCE) plane carries the
/// reference depth.
#[must_use]
pub fn ribbon_slot() -> GeometrySlotSpec {
    let corner = |location: u32| {
        [
            VertexAttribute { location, format: VertexFormat::Float32x3 },
            VertexAttribute { location: location + 1, format: VertexFormat::Uint8x4 },
            VertexAttribute { location: location + 2, format: VertexFormat::Unorm8x4 },
        ]
    };
    let mut layout = corner(0).to_vec();
    layout.extend(corner(3));
    layout.extend([
        VertexAttribute { location: 6, format: VertexFormat::Float32x3 },
        VertexAttribute { location: 7, format: VertexFormat::Float32x2 },
        VertexAttribute { location: 8, format: VertexFormat::Float32x2 },
        VertexAttribute { location: 9, format: VertexFormat::Float32 },
        VertexAttribute { location: 10, format: VertexFormat::Unorm8x4 },
    ]);

    GeometrySlotSpec { layout }
}

/// Bytes one ribbon vertex packs, in [`ribbon_slot`] declaration order.
pub const RIBBON_VERTEX_BYTES: usize = 2 * (12 + 4 + 4) + 12 + 8 + 8 + 4 + 4;

/// The same ribbon with no binding: where the pen already *is*.
///
/// The volatile half is re-solved on the CPU at whatever pose is
/// running, so it arrives posed — and it is the half that travels every
/// frame, which is why the binding it cannot use is worth not sending.
/// [`super::sight::posed_points_slot`] divides for the same reason.
#[must_use]
pub fn posed_ribbon_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 2, format: VertexFormat::Float32x2 },
            VertexAttribute { location: 3, format: VertexFormat::Float32x2 },
            VertexAttribute { location: 4, format: VertexFormat::Unorm8x4 },
        ],
    }
}

/// Bytes one already-posed ribbon vertex packs: where the pen goes, the
/// chord across it, the two field texels, the two eye-free scalars and
/// the ink.
pub const POSED_RIBBON_VERTEX_BYTES: usize = 12 + 12 + 8 + 8 + 4;

/// Uniform window for both passes — the WGSL `StrokeParams` block.
pub struct StrokeUniforms {
    /// The matrix the drawing was solved for.
    pub view_proj: Mat4,
    /// Where the viewer sits, which the prepass' push is measured from.
    pub eye: Vec3,
    /// [`Mesh::surface_bias`] of the subject: the distance the prepass
    /// pushes it away so a stroke lying on it survives the depth test.
    ///
    /// [`Mesh::surface_bias`]: crate::mesh::Mesh::surface_bias
    pub bias: f32,
    /// The field's texel dimensions — the canvas, half this program's
    /// reference extent.
    pub field: (u32, u32),
    /// This frame's pose, as [`deform::bone_uniform`] lays it out — the
    /// same table the field's own blob carries, so the rails are solved
    /// against the surface the verdicts were read off.
    ///
    /// [`deform::bone_uniform`]: crate::deform::bone_uniform
    pub bones: [f32; BONE_LIMIT * 12],
}

impl StrokeUniforms {
    /// Bytes of one `StrokeParams` block: a `mat4x4<f32>`, a
    /// `vec3<f32>`, a `vec2<f32>`, three scalars and the bone table,
    /// rounded to the struct's 16-byte alignment.
    pub const BYTES: u32 = 96 + (BONE_LIMIT * 48) as u32;

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut window = vec![0u8; Self::BYTES as usize];
        for (lane, value) in window[0..64].chunks_exact_mut(4).zip(self.view_proj.to_cols_array()) {
            lane.copy_from_slice(&value.to_le_bytes());
        }
        let tail = [
            self.eye.x,
            self.eye.y,
            self.eye.z,
            self.bias,
            self.field.0 as f32,
            self.field.1 as f32,
            style::RAMP,
            visibility::CHART_COVERAGE,
        ];
        for (lane, value) in window[64..96].chunks_exact_mut(4).zip(tail) {
            lane.copy_from_slice(&value.to_le_bytes());
        }
        for (lane, value) in window[96..].chunks_exact_mut(4).zip(self.bones) {
            lane.copy_from_slice(&value.to_le_bytes());
        }

        window
    }
}

/// One half's ribbons, packed for [`ribbon_slot`].
///
/// Two vertices a point — the two rails — so a segment is the quad
/// between consecutive pairs. The drawing and the layout are the same
/// pair the field was built from, which is what makes the packed texel
/// indices address this point's own verdict and this curve's own
/// reference depth.
///
/// Two buffers, as the points divide into two (#4435), and for the same
/// reason now that the rail solve has crossed to the vertex stage:
/// nothing packed here is a function of the eye, so a resident curve's
/// ribbon is uploaded once per subject and left alone while the camera
/// turns (#4440).
///
/// A curve with no segment at all contributes no vertices and no
/// indices. A curve the *eye* declines — under its class' length floor
/// at this distance — is packed like any other and collapses in the
/// vertex stage instead, since which curves those are is not known
/// until the frame that asks.
///
/// A half that laid out to nothing still packs one stand-in vertex and
/// a triangle of three copies of it, for the reason
/// [`sight::point_vertices`](super::sight::point_vertices) packs a
/// stand-in point: a dispatch supplies one id per declared geometry
/// slot or it warn-drops whole.
///
/// The *region* chooses the layout, not the rig: a geometry slot is
/// declared once when the program registers and cannot ask whether this
/// subject carries one. [`Half::Resident`] packs the anchored layout
/// whether or not `bound` is `Some`, and an unrigged subject's points
/// stand for themselves in an anchorage whose share row is empty.
#[must_use]
pub fn ribbon_geometry(
    drawing: Drawing<'_>,
    layout: &Layout,
    half: Half,
    bound: Option<Bound<'_>>,
) -> (Vec<u8>, Vec<u8>) {
    let anchored_layout = half == Half::Resident;
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    let mut anchored = Vec::new();
    let mut base = 0u32;
    for (index, curve) in drawing.half(half) {
        anchored.clear();
        let Some(span) = layout.span_of(index) else {
            continue;
        };
        if !ribbon::anchors(curve, 0, &mut anchored) {
            continue;
        }

        let colour = ribbon::ink(curve.pen);
        let channels = [colour.r, colour.g, colour.b, 1.0];
        let authored = u32::from(curve.authored);
        for (at, (anchor, point)) in anchored.iter().zip(&curve.points).enumerate() {
            let pen = anchored_layout.then(|| {
                bound
                    .zip(point.anchorage)
                    .map_or_else(|| Anchored::posed(anchor.pos, Vec3::ZERO), |(bound, at)| bound.anchored(at))
            });
            for side in 0..2u32 {
                let address = [((span.start + at as u32) * 4 + authored * 2 + side) as f32, span.start as f32];
                pack(&mut vertices, pen.as_ref(), anchor, address, channels);
            }
        }

        for at in 1..anchored.len() as u32 {
            let (left, right) = (base + 2 * (at - 1), base + 2 * (at - 1) + 1);
            let (next_left, next_right) = (base + 2 * at, base + 2 * at + 1);
            for corner in [left, next_left, next_right, left, next_right, right] {
                indices.extend_from_slice(&corner.to_le_bytes());
            }
        }
        base += 2 * anchored.len() as u32;
    }

    if vertices.is_empty() {
        // The stand-in has to carry the region's own layout, since which
        // of the two the slot was declared with is what the vertex fetch
        // reads it as.
        let inert = Anchor { pos: Vec3::ZERO, along: Vec3::ZERO, half: 0.0, drift: 0.0 };
        let pen = anchored_layout.then(|| Anchored::posed(Vec3::ZERO, Vec3::ZERO));
        pack(&mut vertices, pen.as_ref(), &inert, [0.0, 0.0], [0.0; 4]);
        indices.extend([0u32; 3].into_iter().flat_map(u32::to_le_bytes));
    }

    (vertices, indices)
}

/// One ribbon vertex: where the pen goes — as an address on the sculpt
/// where the region carries one, as a position where it does not — then
/// the rest chord across it, the two field texels, the two eye-free
/// scalars, and the ink.
///
/// The chord travels as the rest curve's and is turned by the point's
/// own blend in the vertex stage rather than being posed corner by
/// corner. It is a *direction*, and the only thing the rail solve asks
/// of it is a cross product with the view — so the two corners can
/// disagree about how to turn it only by the weight gradient across one
/// triangle edge, which is a fraction of a degree on a mesh of this
/// density and reaches the picture nowhere.
fn pack(vertices: &mut Vec<u8>, pen: Option<&Anchored>, anchor: &Anchor, address: [f32; 2], channels: [f32; 4]) {
    let unorm = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    match pen {
        Some(pen) => {
            for corner in 0..2usize {
                vertices.extend(pen.positions[corner].to_array().into_iter().flat_map(f32::to_le_bytes));
                vertices.extend(pen.joints[corner]);
                vertices.extend(pen.shares[corner].map(unorm));
            }
        }
        None => vertices.extend(anchor.pos.to_array().into_iter().flat_map(f32::to_le_bytes)),
    }
    let scalars = [anchor.along.x, anchor.along.y, anchor.along.z, address[0], address[1], anchor.half, anchor.drift];
    vertices.extend(scalars.into_iter().flat_map(f32::to_le_bytes));
    if let Some(pen) = pen {
        vertices.extend(pen.between.to_le_bytes());
    }
    vertices.extend(channels.map(unorm));
}

/// Where the ink sheet is created, in texels, for a canvas of `canvas`.
#[must_use]
pub fn sheet_size(canvas: (usize, usize)) -> (usize, usize) {
    (canvas.0 * SUPERSAMPLE as usize, canvas.1 * SUPERSAMPLE as usize)
}

/// The whole ink pass as one register graph.
///
/// Four passes: the two ribbon halves into a raster transient — the
/// resident one clearing it, the volatile one loading what it left —
/// then the wash's ink coverage plane reduced out of that raster, then
/// the resolve that turns it into the sheet the frame composites.
///
/// # The coverage plane, and why it is derived here
///
/// The wash yields boundary duty to the ink: its flow field runs along
/// the strokes, and its tide lines stop at them. What it needs to know
/// is where ink stands — the drawing's own alpha rather than its colour
/// — and that is exactly what the raster above holds, one frame at a
/// time, with every hidden point already collapsed by the field. So
/// `fs_ink_plane` reduces it: one texel of the wash body's extent takes
/// the greatest alpha over the [`INK_PLANE_FOOTPRINT`]-square block of
/// raster texels it covers, which claims a body texel exactly when a
/// stroke passes anywhere through it (iamacoffeepot/aether#4451).
///
/// That the reduction is a maximum rather than a mean is the whole of
/// it. Most of the drawing is thinner than one body texel, and a mean —
/// or a single tap — would find a stroke only where it happened to pass
/// near a sample, so the flow would be solved from a dashed drawing and
/// report a confident orientation off the wrong lines. The maximum is
/// also what the CPU rasterization it replaces meant by its half-pixel
/// slack: claim the texel the stroke touches, not the texel whose centre
/// it happens to cover.
///
/// It sits before the resolve because the resolve writes the reference
/// binding, and the last pass' output binding is what states the
/// reference extent.
///
/// # There is no depth here, and that is a deviation from ADR-0172
///
/// The record has the ink depth-tested against a prepass of the
/// subject. Measured on the shipped drawing, that prepass removes
/// nearly half the ink: `px < 120` on the pinned framing came to 18,706
/// against the CPU path's 31,982, and dropping the depth test alone
/// took it to 35,862 — main's own weight and shape.
///
/// The reason is structural rather than a tuning miss. The drawing's
/// points are level-set crossings *on* the surface, so every stroke is
/// coplanar with the thing it would be tested against — the degenerate
/// case for a depth comparison — and a `Depth32Float` buffer spanning
/// 0.05 to 40 has little precision left at the framing's distance of
/// 5.4. Pushing the subject back by the occlusion bias moves the
/// fight around without settling it.
///
/// It is also asking a question already answered. Subject occlusion is
/// what the visibility field is *for*: it is decided per point, against
/// the oracle's own ray semantics, and gated by a parity test. Asking
/// it a second time per fragment through a worse instrument can only
/// disagree. The CPU path this replaces never depth-tested against the
/// subject either — the mesh is never drawn — so leaving it out is
/// parity with the drawing it has to match, not a shortcut.
///
/// What is given up is stroke-versus-stroke ordering, which the world
/// pass used to supply. Ribbons now composite in draw order. On this
/// drawing that is not visible, and the measurement above is the
/// evidence: the histogram tracks the CPU path's across every band.
///
/// The resolve is last because it has to be — the final pass writes the
/// binding whose texture states the reference extent, and that is the
/// supersampled sheet. It is not a pass spent on nothing: the
/// conversion it performs is what keeps the composite from laying down
/// a quarter of the ink at a half-covered pixel.
#[must_use]
pub fn program() -> ProgramRegister {
    let planes = [SEEN, REACH, COVERAGE, TOTAL, REFERENCE].map(|index| InputSlot::Binding { index }).to_vec();
    let ribbons = |entry: &str, geometry: u32, load: PassLoad| ProgramPass {
        stage: PassStage::Draw(DrawPass { vertex_entry_point: entry.to_owned(), geometry, depth: None, load }),
        entry_point: "fs_stroke".to_owned(),
        inputs: planes.clone(),
        output: OutputSlot::Transient { index: RASTER },
        uniform_offset: 0,
        uniform_length: StrokeUniforms::BYTES,
        repeat: None,
    };
    let off_the_raster = |entry_point: &str, output: OutputSlot| ProgramPass {
        stage: PassStage::Fragment,
        entry_point: entry_point.to_owned(),
        inputs: vec![InputSlot::Transient { index: RASTER }],
        output,
        uniform_offset: 0,
        uniform_length: StrokeUniforms::BYTES,
        repeat: None,
    };
    let passes = vec![
        ribbons("vs_stroke", RESIDENT, PassLoad::Clear),
        ribbons("vs_stroke_posed", VOLATILE, PassLoad::Load),
        off_the_raster("fs_ink_plane", OutputSlot::Binding { index: INK_PLANE }),
        off_the_raster("fs_resolve", OutputSlot::Binding { index: INK }),
    ];

    let mut bindings = vec![plane_slot(); BINDING_COUNT - 2];
    bindings.push(ink_slot());
    bindings.push(ink_plane_slot());

    ProgramRegister {
        // The skinning prelude after this module's own source: the rail
        // solve stands on a pen posed from the same bone table the
        // field's verdicts were read through.
        wgsl: format!("{STROKE_WGSL}\n{SKIN_WGSL}"),
        bindings,
        transients: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        geometries: vec![ribbon_slot(), posed_ribbon_slot()],
        depth_transients: Vec::new(),
        passes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::easel::program::sight;
    use crate::feature::{Curve3, FeatureClass, Pen, SurfacePoint};
    use aether_render::vertex_stride_bytes;

    /// Tripwire: the packed stride is what the register checks the
    /// vertex stage against, and a lane that slides here reads the
    /// wrong field texel for every point past the first — a failure
    /// that renders as a plausible drawing of the wrong occlusion.
    #[test]
    fn the_packed_vertex_matches_the_declared_layout() {
        assert_eq!(vertex_stride_bytes(&ribbon_slot().layout), RIBBON_VERTEX_BYTES, "the bound half's stride");
        assert_eq!(
            vertex_stride_bytes(&posed_ribbon_slot().layout),
            POSED_RIBBON_VERTEX_BYTES,
            "the posed half's stride",
        );
    }

    /// Tripwire: each half's indices address its own buffer.
    ///
    /// The two halves are separate geometries, so a vertex counter that
    /// ran on across the split would leave the volatile half indexing
    /// past its own end — which a driver answers with whatever it
    /// likes, not with an error.
    #[test]
    fn each_half_indexes_only_its_own_vertices() {
        let resident = [curve(1, 0.0), curve(2, 0.4)];
        let volatile = [curve(3, 0.8)];
        let drawing = Drawing { resident: &resident, volatile: &volatile };
        let layout = sight::layout(drawing, (64, 64)).expect("fits");

        for half in [Half::Resident, Half::Volatile] {
            let (vertices, indices) = ribbon_geometry(drawing, &layout, half, None);
            let count = (vertices.len() / POSED_RIBBON_VERTEX_BYTES) as u32;
            for corner in indices.chunks_exact(4) {
                let at = u32::from_le_bytes(corner.try_into().expect("four bytes"));
                assert!(at < count, "{half:?} indexes vertex {at} of {count}");
            }
        }
    }

    fn curve(seed: u64, at: f32) -> Curve3 {
        Curve3 {
            points: (0..6)
                .map(|i| SurfacePoint::on_surface(Vec3::new(at + i as f32 * 0.05, 0.0, 0.0), Vec3::new(0.0, 0.0, 1.0)))
                .collect(),
            class: FeatureClass::Silhouette,
            pen: Pen::Ink,
            seed,
            authored: false,
        }
    }
}
