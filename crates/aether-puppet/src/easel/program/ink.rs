//! The ink coverage plane as an authored draw pass (iamacoffeepot/aether#4410).
//!
//! Where the drawing itself landed, re-spoken as a rasterizing pass over
//! resident ribbon geometry (ADR-0171). The CPU code stays the oracle:
//! [`regions::ink`](crate::easel::regions::ink) still bakes the plane the
//! flow field reads, and this pass develops the same plane beside it
//! until #4412 moves the consumers across.
//!
//! The module is data plus builders, the shape every sibling op module
//! takes: [`INK_WGSL`] carries the entry points, [`coverage_pass`]
//! returns the `ProgramPass` that invokes them, [`geometry_slot`] names
//! the vertex layout the pass binds, and [`InkUniforms`] encodes the
//! uniform window byte for byte against the WGSL `InkUniforms` block.
//!
//! Unlike every other op here, parity is exact rather than thresholded.
//! The oracle is a binary test — a pixel is claimed or it is not — so
//! there is no accumulator to drift and no quantization to absorb. The
//! fragment stage runs the oracle's own three edge functions at the
//! oracle's own sample point, and the vertex stage only has to widen each
//! triangle enough that the hardware offers those pixels for the test.
//!
//! # Single-sample, deliberately
//!
//! Program and draw passes are single-sample by construction (ADR-0171;
//! the frame's own world and overlay passes are 4x MSAA and resolve).
//! That is the right shape here for the same reason it is right for the
//! plane bake: the oracle takes one sample at each pixel's centre and
//! answers yes or no, so there is no coverage fraction for multisampling
//! to recover. What the structure tensor downstream asks of this plane
//! is where a stroke is and which way it runs, and a half-claimed pixel
//! would answer neither more truthfully.

use aether_math::{Mat4, Vec2, Vec3};
use aether_render::{
    DrawPass, DrawTriangle, GeometrySlotSpec, OutputSlot, PassLoad, PassStage, ProgramPass, SlotExtent, SlotSpec,
    TextureFormat, VertexAttribute, VertexFormat,
};

/// The ink pass' own WGSL. Never registered alone — the wash program's
/// [`module`](super::wash::module) concatenates it with the op modules.
pub const INK_WGSL: &str = include_str!("ink.wgsl");

/// The vertex layout the coverage pass binds: a triangle's three
/// world-space corners on every one of its vertices, in cyclic order
/// starting at the vertex's own.
///
/// The fragment stage needs all three corners to evaluate the oracle's
/// edge functions, and a vertex stage has no way to reach its siblings,
/// so each vertex carries the whole triangle. Cyclic rather than
/// absolute order lets the vertex stage widen its own corner against its
/// two neighbours without knowing which of the three it is, and leaves
/// the winding — and so the flat-interpolated `(a, b, c)` the fragment
/// stage reads from the provoking vertex — identical to the oracle's.
#[must_use]
pub fn geometry_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 2, format: VertexFormat::Float32x3 },
        ],
    }
}

/// Uniform window for `vs_ink` / `fs_ink` — the WGSL `InkUniforms` block.
///
/// The view-projection rides the window rather than a plane because the
/// vertex stage is what consumes it, and a draw pass' window binds to the
/// vertex stage as well as the fragment stage (ADR-0171).
pub struct InkUniforms {
    /// The matrix the ribbons were solved for, so the coverage registers
    /// with the sheet the flow is applied to.
    pub view_proj: Mat4,
    /// Half the canvas size in pixels — the projection's page mapping.
    pub half_size: Vec2,
}

impl InkUniforms {
    /// 64 for the matrix, 8 for the half-size, 8 of tail padding: a WGSL
    /// uniform block rounds to its own 16-byte alignment.
    pub const BYTES: u32 = 80;

    #[must_use]
    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        for (at, value) in self.view_proj.to_cols_array().into_iter().enumerate() {
            bytes[at * 4..at * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[64..68].copy_from_slice(&self.half_size.x.to_le_bytes());
        bytes[68..72].copy_from_slice(&self.half_size.y.to_le_bytes());
        bytes
    }
}

/// The pass that bakes the coverage plane: one indexed triangle-list
/// draw of the bound ribbon geometry, cleared first so a develop never
/// shows the last one's strokes.
///
/// No depth transient, for the reason the oracle keeps no depth buffer —
/// a ribbon hidden behind another still says which way the lock it
/// belongs to falls.
#[must_use]
pub fn coverage_pass(geometry: u32, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Draw(DrawPass {
            vertex_entry_point: "vs_ink".to_owned(),
            geometry,
            depth: None,
            load: PassLoad::Clear,
        }),
        entry_point: "fs_ink".to_owned(),
        inputs: Vec::new(),
        output,
        uniform_offset,
        uniform_length: InkUniforms::BYTES,
        repeat: None,
    }
}

/// Bytes one vertex occupies: the triangle's three corners, three
/// 32-bit floats each. Attributes pack in declaration order with no
/// padding (ADR-0171), so this is the sum of the formats' widths and
/// [`vertices`] writes them in exactly this order.
pub const VERTEX_BYTES: usize = 12 + 12 + 12;

/// The drawing as corner triples, with an empty one standing in as a
/// single triangle collapsed to a point.
///
/// A develop has to fill its geometry slot whatever it drew — a dispatch
/// supplies one id per declared slot or it warn-drops whole — so an empty
/// drawing neutralizes through the content, never by restructuring the
/// graph. The vertex stage's area floor culls the stand-in exactly as
/// the oracle skips a degenerate triangle.
fn corners(triangles: &[DrawTriangle]) -> Vec<[Vec3; 3]> {
    match triangles {
        [] => vec![[Vec3::new(0.0, 0.0, 0.0); 3]],
        drawn => drawn.iter().map(|at| at.verts.map(|v| Vec3::new(v.x, v.y, v.z))).collect(),
    }
}

/// The ribbon vertex buffer, packed for [`geometry_slot`]: three
/// vertices per triangle, each carrying the whole triangle from its own
/// corner round.
///
/// Called per develop, not per subject. The drawing is solved fresh for
/// every eye, and a posed mesh will solve it per frame, so the honest
/// re-upload is the whole buffer through `aether.render.update_geometry`
/// — the path ADR-0171 blesses for view-dependent geometry that is
/// small by nature.
#[must_use]
pub fn vertices(triangles: &[DrawTriangle]) -> Vec<u8> {
    let corners = corners(triangles);
    let mut packed = Vec::with_capacity(corners.len() * 3 * VERTEX_BYTES);
    for [a, b, c] in corners {
        for rotation in [[a, b, c], [b, c, a], [c, a, b]] {
            for corner in rotation {
                for value in corner.to_array() {
                    packed.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
    }

    packed
}

/// The ribbon index buffer: sequential little-endian `u32` triangle-list
/// indices.
///
/// Nothing is shared. The ribbons arrive as an unshared triangle soup,
/// and a vertex's attributes depend on which corner of which triangle it
/// is, so two coincident corners are still two vertices.
#[must_use]
pub fn indices(triangles: &[DrawTriangle]) -> Vec<u8> {
    let count = u32::try_from(corners(triangles).len() * 3).unwrap_or(u32::MAX);
    (0..count).flat_map(u32::to_le_bytes).collect()
}

/// One coverage plane's slot: full-extent `R32Float`, the shape
/// [`super::wash::program`] lays the pass' transient as and the shape a
/// consumer binds it at.
#[must_use]
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_math::Rgb;
    use aether_render::{Vertex, vertex_stride_bytes};

    fn triangle() -> DrawTriangle {
        let vertex = |x: f32| Vertex { x, y: 0.0, z: 0.0, color: Rgb::new(0.0, 0.0, 0.0) };
        DrawTriangle { verts: [vertex(0.0), vertex(1.0), vertex(2.0)] }
    }

    /// Tripwire: the packer and the declared layout must agree on the
    /// stride. They are two independent statements of one byte
    /// arrangement — the layout is read by the register to build the
    /// vertex buffer layout, the packer writes the bytes — and a
    /// disagreement is not a compile error but a silently reinterpreted
    /// drawing, every corner sliding one lane per vertex.
    #[test]
    fn the_packed_vertex_matches_the_declared_stride() {
        assert_eq!(vertex_stride_bytes(&geometry_slot().layout), VERTEX_BYTES, "declared stride");

        let drawing = [triangle(), triangle()];
        assert_eq!(vertices(&drawing).len(), drawing.len() * 3 * VERTEX_BYTES, "packed length");
        assert_eq!(indices(&drawing).len(), drawing.len() * 3 * 4, "index length");
    }

    /// Tripwire: an empty drawing still has to produce a geometry a
    /// dispatch can name, or the develop warn-drops whole and the sheet
    /// silently stops updating.
    #[test]
    fn an_empty_drawing_still_packs_one_triangle() {
        assert_eq!(vertices(&[]).len(), 3 * VERTEX_BYTES);
        assert_eq!(indices(&[]).len(), 3 * 4);
    }
}
