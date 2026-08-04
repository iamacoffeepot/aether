//! The painter's input maps as an authored draw pass
//! (iamacoffeepot/aether#4411, #4412, ADR-0171).
//!
//! [`super::super::regions::rasterize`] walks 434k faces on the CPU to
//! answer three questions per pixel — what material is this, how lit is
//! it, how far does it turn toward the eye — and that walk is why the
//! easel repaints on a settle gate instead of on the frame. The three
//! answers are all *blends of per-vertex quantities under barycentric
//! weights*, which is the one thing a rasterizer does natively, so this
//! module hands the whole bake to the hardware: the subject uploads as
//! a geometry whose vertex layout carries the blurred class indicators
//! ([`crate::labels::Labels::vertex_scores`]) and the key-light term alongside
//! position and normal, and one draw pass interpolates them into the
//! three channels of one [`packed_slot`] plane.
//!
//! # One pass, three channels
//!
//! The three answers come off one interpolated surface, so they cost one
//! rasterization, not three. ADR-0171 declines multiple render targets on
//! exactly that ground — one pass filling one target's channels already
//! covers every consumer — and the offline bake proved the packing (R
//! class, G tone, B facing) long before this module existed. Filling
//! three single-channel `R32Float` planes from three passes instead put
//! the subject through the rasterizer three times for one surface: 10.7
//! ms per frame at 900x1200 against the 4.0 ms the packed pass costs.
//!
//! The channel contract lives in `bake.wgsl`'s header, next to the code
//! that writes it. In brief: the class rides as `class / 255`, which an
//! 8-bit unorm carries exactly and returns as the same integer so long as
//! nothing linear-filters it; tone and facing quantize to about one part
//! in 255, which is the quantization the parity readback already carried.
//! Tone clips at one, and every consumer saturates below that.
//!
//! The classification fit is exact rather than analogous. Spike 142's
//! rule is to blur each class's indicator volume, sample the blurred
//! scores at the vertices, interpolate those across the face, and
//! argmax only at the end — never a label lookup at the pixel's own
//! point, which on a thin shell paints an ear's outer sheet with the
//! concha's class (issue 4399). Interpolate-then-argmax *is* the
//! rasterizer's interpolator followed by eight comparisons in the
//! fragment stage, so the GPU form is the definition, not an
//! approximation of it.
//!
//! # What this module is not
//!
//! It does not develop the wash. The easel still bakes its planes on
//! the CPU and paints from those; this is the bake alone, held against
//! the oracle by `tests/program_bake_scenario.rs` so the switch-over
//! lands on a proven surface rather than proving itself in flight. The
//! switch itself waits on the class plane's other CPU consumers — the
//! care field, the accents, and the centroids that place every wash's
//! accidents — all of which move together in
//! iamacoffeepot/aether#4387.
//!
//! # Single-sample, deliberately
//!
//! Program and draw passes are single-sample by construction (ADR-0171;
//! the frame's own world and overlay passes are 4x MSAA and resolve).
//! That is the right shape here and would be the right shape even if it
//! were a knob: the oracle takes one sample at each pixel's centre, and
//! a resolved class plane is worse than useless — averaging the labels
//! `3` and `8` across a silhouette yields `5`, a class neither surface
//! carries. What the planes want at an edge is one honest answer, and
//! the wash softens its own edges downstream anyway.

use aether_math::{Mat4, Vec3};
use aether_render::{
    DrawPass, GeometrySlotSpec, OutputSlot, PassLoad, PassStage, ProgramPass, ProgramRegister, SlotExtent, SlotSpec,
    TextureFormat, VertexAttribute, VertexFormat,
};

use crate::extract::Settings;
use crate::feature::SurfacePoint;
use crate::labels::CLASSES;
use crate::mesh::Mesh;

/// The bake's WGSL: one vertex entry feeding one fragment entry.
pub const BAKE_WGSL: &str = include_str!("bake.wgsl");

/// The dispatch binding [`program`] declares — the one packed plane a
/// dispatch supplies and the pass fills.
pub const PACKED: u32 = 0;

/// The vertex layout splits the eight class scores three-three-two
/// because `VertexFormat` has no four-lane float (ADR-0171's closed
/// set). A ninth class would want its own attribute rather than a
/// silent repack, so the split is pinned to the class count here.
const _: () = assert!(CLASSES == 8, "the vertex layout packs exactly eight class indicators as 3 + 3 + 2 lanes");

/// Bytes one vertex occupies: position, normal, tone, then the eight
/// indicators. Attributes pack in declaration order with no padding
/// (ADR-0171), so this is the sum of the formats' widths and
/// [`vertices`] writes them in exactly this order.
pub const VERTEX_BYTES: usize = 12 + 12 + 4 + 12 + 12 + 8;

/// The geometry slot the draw pass binds: the subject, carrying
/// everything the three channels blend.
///
/// Locations match `bake.wgsl`'s `vs_bake` parameters. `tone` is a
/// baked attribute rather than a shader derivation because the oracle
/// evaluates [`Settings::tone`] per vertex and blends the result —
/// carrying the same scalar makes the tone plane parity by
/// construction, and spares the shader a second transcription of the
/// face lift's falloff.
pub fn geometry_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 2, format: VertexFormat::Float32 },
            VertexAttribute { location: 3, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 4, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 5, format: VertexFormat::Float32x2 },
        ],
    }
}

/// The baked plane's slot: full-extent `Rgba8`, carrying class, tone and
/// facing in R, G and B.
///
/// A texture bound here must be created `TextureSampling::Nearest`. The
/// class channel is an integer in disguise, and a linear filter across a
/// material boundary averages the labels either side into a third the
/// surface never carried. Consumers reaching it through `textureLoad`
/// take no sampler and are safe either way; the create is where the
/// guarantee is cheap to make and expensive to omit.
pub fn packed_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }
}

/// Uniform window for every pass — the WGSL `BakeParams` block.
///
/// Both fields are per-frame by nature, which is the point: a subject
/// that turns, or a camera that moves, changes only this blob. The
/// geometry is re-uploaded only when the vertices themselves move.
pub struct BakeUniforms {
    /// The camera the ink was drawn from. The maps have to register
    /// with the drawing to the pixel, so this is the drawing's own
    /// matrix rather than one derived alongside it.
    pub view_proj: Mat4,
    /// Where the viewer sits — what `facing` asks about.
    pub eye: Vec3,
}

impl BakeUniforms {
    /// Bytes of the `BakeParams` block: a `mat4x4<f32>` then a
    /// `vec3<f32>`, rounded out to the struct's 16-byte alignment.
    pub const BYTES: u32 = 80;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut window = [0u8; Self::BYTES as usize];
        for (lane, value) in window[0..64].chunks_exact_mut(4).zip(self.view_proj.to_cols_array()) {
            lane.copy_from_slice(&value.to_le_bytes());
        }
        for (lane, value) in window[64..76].chunks_exact_mut(4).zip(self.eye.to_array()) {
            lane.copy_from_slice(&value.to_le_bytes());
        }

        window
    }
}

/// The subject's vertex buffer, packed for [`geometry_slot`].
///
/// `scores` is the per-vertex blurred-indicator matrix the oracle
/// interpolates ([`crate::labels::Labels::vertex_scores`]), indexed the same
/// way `mesh.positions` is.
///
/// Called per frame, not per subject. Nothing here is cached and
/// nothing is keyed on the mesh being the one from last frame: a pose
/// that moves a vertex moves its normal and its key-light term with it,
/// so the honest re-upload is the whole buffer through
/// `aether.render.update_geometry`. ADR-0171 prefers a pose to ride the
/// uniform blob where it can, and the layout leaves that door open —
/// skinning joints and weights are expressible in the same closed
/// format set — but a re-upload has to work first, and this is it.
pub fn vertices(mesh: &Mesh, scores: &[[f32; CLASSES]], settings: &Settings) -> Vec<u8> {
    let mut packed = Vec::with_capacity(mesh.positions.len() * VERTEX_BYTES);
    for (index, (&position, &normal)) in mesh.positions.iter().zip(&mesh.normals).enumerate() {
        let tone = settings.tone(&SurfacePoint::on_surface(position, normal));
        let indicators = scores.get(index).copied().unwrap_or([0.0; CLASSES]);

        for value in position.to_array().into_iter().chain(normal.to_array()).chain([tone]).chain(indicators) {
            packed.extend_from_slice(&value.to_le_bytes());
        }
    }

    packed
}

/// The subject's index buffer: the face list as little-endian `u32`
/// triangle-list indices.
pub fn indices(mesh: &Mesh) -> Vec<u8> {
    mesh.faces.iter().flatten().flat_map(|corner| corner.to_le_bytes()).collect()
}

/// The whole bake as one register graph: one draw pass over one
/// geometry, through one depth slot and one uniform window.
///
/// Static by construction, like the wash's own graph — the structure
/// depends on nothing but the plane vocabulary, so it is the same graph
/// for every subject at every canvas size.
pub fn program() -> ProgramRegister {
    ProgramRegister {
        wgsl: BAKE_WGSL.to_owned(),
        bindings: vec![packed_slot()],
        transients: Vec::new(),
        geometries: vec![geometry_slot()],
        // The depth slot resolves which surface each pixel's answers come
        // from. One pass needs it no less than three did: the subject is
        // a closed shell, so most pixels are covered front and back.
        depth_transients: vec![SlotExtent::Full],
        passes: vec![ProgramPass {
            stage: PassStage::Draw(DrawPass {
                vertex_entry_point: "vs_bake".to_owned(),
                geometry: 0,
                depth: Some(0),
                load: PassLoad::Clear,
            }),
            entry_point: "fs_packed".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: PACKED },
            uniform_offset: 0,
            uniform_length: BakeUniforms::BYTES,
            repeat: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_render::vertex_stride_bytes;

    /// Tripwire: the packer and the declared layout must agree on the
    /// stride. They are two independent statements of one byte
    /// arrangement — the layout is read by the register to build the
    /// vertex buffer layout, the packer writes the bytes — and a
    /// disagreement is not a compile error but a silently reinterpreted
    /// mesh, every attribute sliding one lane per vertex.
    #[test]
    fn the_packed_vertex_matches_the_declared_stride() {
        assert_eq!(vertex_stride_bytes(&geometry_slot().layout), VERTEX_BYTES, "declared stride");

        let mesh = Mesh::from_obj_bytes(b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n", 0).expect("fixture mesh");
        let scores = vec![[0.0; CLASSES]; mesh.positions.len()];

        assert_eq!(
            vertices(&mesh, &scores, &Settings::default()).len(),
            mesh.positions.len() * VERTEX_BYTES,
            "packed length",
        );
    }
}
