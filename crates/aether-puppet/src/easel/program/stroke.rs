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
//! Every curve is rasterized whole. The stage reads the point's
//! verdict, reach, run arc and curve coverage out of the field, folds
//! them into one width scale, and displaces the vertex along the rail
//! offset the CPU solved. Hidden points scale to zero and their
//! segments rasterize nothing, so the runs the CPU used to cut are not
//! cut anywhere — they are what is left when the hidden widths vanish.
//!
//! # What stays on the CPU
//!
//! The rail solve: the eye-facing perpendicular, the depth weighting,
//! and the hand-wobble, all of which are per-point and view-dependent
//! and none of which the field knows about. They are packed as vertex
//! attributes at re-split cadence, exactly as ADR-0172 has it.

use aether_math::{Mat4, Vec3};
use aether_render::{
    DrawPass, GeometrySlotSpec, InputSlot, OutputSlot, PassLoad, PassStage, ProgramPass, ProgramRegister, SlotExtent,
    SlotSpec, TextureFormat, VertexAttribute, VertexFormat,
};

use super::sight::Layout;
use crate::feature::Curve3;
use crate::ribbon::{self, Rail};
use crate::style;
use crate::visibility;

/// The ink pass' own WGSL: the subject prepass and the ribbon stage.
pub const STROKE_WGSL: &str = include_str!("stroke.wgsl");

/// Dispatch-binding indices, in the order [`program`] declares them.
///
/// The four planes are the field's own bindings, supplied by naming the
/// same texture ids the sight dispatch wrote — they resolve at
/// `Divided { divisor: 2 }` against this program's doubled reference.
pub const SEEN: u32 = 0;
pub const REACH: u32 = 1;
pub const COVERAGE: u32 = 2;
pub const TOTAL: u32 = 3;
/// The supersampled ink sheet, and so the reference extent.
pub const INK: u32 = 4;

/// How many bindings a dispatch supplies.
pub const BINDING_COUNT: usize = 5;

/// Geometry slot index: the drawing's ribbons, and nothing else. The
/// subject is not rasterized here — see [`program`].
pub const RIBBON: u32 = 0;

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

/// The ribbon geometry's layout: the rail centre, the offset that
/// reaches the rail at full pressure, the field texel the point owns,
/// its ink colour, and whether the mark was authored.
///
/// The offset is carried rather than a direction and a width because
/// the vertex stage's whole contribution is one scalar — the scale it
/// reads from the field — and a vertex that scales to zero must land
/// exactly on its partner so the segment collapses rather than folding.
#[must_use]
pub fn ribbon_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 2, format: VertexFormat::Float32 },
            VertexAttribute { location: 3, format: VertexFormat::Unorm8x4 },
            VertexAttribute { location: 4, format: VertexFormat::Float32 },
        ],
    }
}

/// Bytes one ribbon vertex packs, in [`ribbon_slot`] declaration order.
pub const RIBBON_VERTEX_BYTES: usize = 12 + 12 + 4 + 4 + 4;

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
}

impl StrokeUniforms {
    /// Bytes of one `StrokeParams` block: a `mat4x4<f32>`, a
    /// `vec3<f32>`, a `vec2<f32>` and three scalars, rounded to the
    /// struct's 16-byte alignment.
    pub const BYTES: u32 = 96;

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

        window
    }
}

/// The drawing's ribbons, packed for [`ribbon_slot`].
///
/// Two vertices a point — the two rails — so a segment is the quad
/// between consecutive pairs. The curve list and the layout are the
/// same pair the field was built from, which is what makes the packed
/// texel index address this point's own verdict.
///
/// A curve the rail solve declines (under two points, or under its
/// class' length floor) contributes no vertices and no indices. Its
/// field span stays allocated and unread, which costs texels rather
/// than correctness.
///
/// Vertices and indices come back together because the rail solve is
/// the expensive half and runs every frame the eye moves — asking for
/// the two separately would solve the whole drawing twice.
#[must_use]
pub fn ribbon_geometry(curves: &[Curve3], layout: &Layout, eye: Vec3) -> (Vec<u8>, Vec<u8>) {
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    let mut solved = Vec::new();
    let mut base = 0u32;
    for (index, curve) in curves.iter().enumerate() {
        solved.clear();
        let Some(span) = layout.span_of(index) else {
            continue;
        };
        if !ribbon::rails(curve, eye, 0, &mut solved) {
            continue;
        }

        let colour = ribbon::ink(curve.pen);
        let channels = [colour.r, colour.g, colour.b, 1.0];
        let authored = if curve.authored {
            1.0f32
        } else {
            0.0
        };
        for (at, rail) in solved.iter().enumerate() {
            let slot = (span.start + at as u32) as f32;
            for side in [-1.0f32, 1.0] {
                vertices.extend_from_slice(&pack(rail, side, slot, channels, authored));
            }
        }

        for at in 1..solved.len() as u32 {
            let (left, right) = (base + 2 * (at - 1), base + 2 * (at - 1) + 1);
            let (next_left, next_right) = (base + 2 * at, base + 2 * at + 1);
            for corner in [left, next_left, next_right, left, next_right, right] {
                indices.extend_from_slice(&corner.to_le_bytes());
            }
        }
        base += 2 * solved.len() as u32;
    }

    (vertices, indices)
}

fn pack(rail: &Rail, side: f32, slot: f32, channels: [f32; 4], authored: f32) -> [u8; RIBBON_VERTEX_BYTES] {
    let mut vertex = [0u8; RIBBON_VERTEX_BYTES];
    let scalars = [
        rail.centre.x,
        rail.centre.y,
        rail.centre.z,
        rail.offset.x * side,
        rail.offset.y * side,
        rail.offset.z * side,
        slot,
    ];
    for (lane, value) in vertex[0..28].chunks_exact_mut(4).zip(scalars) {
        lane.copy_from_slice(&value.to_le_bytes());
    }
    for (lane, value) in vertex[28..32].iter_mut().zip(channels) {
        *lane = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    vertex[32..36].copy_from_slice(&authored.to_le_bytes());

    vertex
}

/// Where the ink sheet is created, in texels, for a canvas of `canvas`.
#[must_use]
pub fn sheet_size(canvas: (usize, usize)) -> (usize, usize) {
    (canvas.0 * SUPERSAMPLE as usize, canvas.1 * SUPERSAMPLE as usize)
}

/// The whole ink pass as one register graph.
///
/// Two passes: the ribbons into a raster transient, then the resolve
/// that turns it into the sheet the frame composites.
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
    let planes = [SEEN, REACH, COVERAGE, TOTAL].map(|index| InputSlot::Binding { index }).to_vec();
    let passes = vec![
        ProgramPass {
            stage: PassStage::Draw(DrawPass {
                vertex_entry_point: "vs_stroke".to_owned(),
                geometry: RIBBON,
                depth: None,
                load: PassLoad::Clear,
            }),
            entry_point: "fs_stroke".to_owned(),
            inputs: planes,
            output: OutputSlot::Transient { index: RASTER },
            uniform_offset: 0,
            uniform_length: StrokeUniforms::BYTES,
            repeat: None,
        },
        ProgramPass {
            stage: PassStage::Fragment,
            entry_point: "fs_resolve".to_owned(),
            inputs: vec![InputSlot::Transient { index: RASTER }],
            output: OutputSlot::Binding { index: INK },
            uniform_offset: 0,
            uniform_length: StrokeUniforms::BYTES,
            repeat: None,
        },
    ];

    let mut bindings = vec![plane_slot(); BINDING_COUNT - 1];
    bindings.push(ink_slot());

    ProgramRegister {
        wgsl: STROKE_WGSL.to_owned(),
        bindings,
        transients: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        geometries: vec![ribbon_slot()],
        depth_transients: Vec::new(),
        passes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the packed stride is what the register checks the
    /// vertex stage against, and a lane that slides here reads the
    /// wrong field texel for every point past the first — a failure
    /// that renders as a plausible drawing of the wrong occlusion.
    #[test]
    fn the_packed_vertex_matches_the_declared_layout() {
        let stride: usize = ribbon_slot()
            .layout
            .iter()
            .map(|attribute| match attribute.format {
                VertexFormat::Float32x3 => 12,
                VertexFormat::Float32x2 => 8,
                VertexFormat::Float32 | VertexFormat::Unorm8x4 | VertexFormat::Uint8x4 => 4,
            })
            .sum();

        assert_eq!(stride, RIBBON_VERTEX_BYTES);
    }
}
