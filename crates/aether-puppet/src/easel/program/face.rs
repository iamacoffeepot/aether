//! The face paint as authored passes (iamacoffeepot/aether#4387,
//! ADR-0171): [`accent`] re-spoken as one draw pass
//! over the chart's aperture loops and five pointwise passes over what it
//! leaves.
//!
//! The CPU module stays the oracle and keeps the law both sides obey — an
//! accent asks the chart where a feature *is*, never the label plane.
//! That is why so little of it moves: everything the chart owns is solved
//! on the CPU, over two dozen points per eye, and arrives here as
//! [`EyeUniform`]s and as the aperture triangles themselves. What the GPU
//! does is the per-pixel half, which is the half that scaled with the
//! canvas.
//!
//! `face.wgsl` carries the op-by-op mapping; this module owns the
//! entry-point names, the uniform layout, the aperture's vertex layout
//! and packers, and the pass builders the coat sequencer composes.

use aether_math::Vec2;
use aether_render::{
    DrawPass, GeometrySlotSpec, InputSlot, OutputSlot, PassLoad, PassStage, ProgramPass, SlotExtent, SlotSpec,
    TextureFormat, VertexAttribute, VertexFormat,
};

use crate::easel::accent::{self, Eye};

/// The face WGSL. Never registered alone — the wash program's
/// [`module`](super::wash::module) concatenates it with its siblings.
pub const FACE_WGSL: &str = include_str!("face.wgsl");

/// Entry points, in the order [`super::wash::program`] lays them.
pub const APERTURE_VERTEX_ENTRY: &str = "vs_aperture";
pub const APERTURE_ENTRY: &str = "fs_aperture";
pub const IRIS_ENTRY: &str = "fs_iris";
/// The lid's weight over the iris. Named for what it produces rather
/// than for what consumes it: `wash.wgsl`'s `fs_lift` is the pass that
/// *applies* this plane to the finished iris density, and the two share
/// one module once [`module`](super::wash::module) concatenates them.
pub const LID_WEIGHT_ENTRY: &str = "fs_lid_weight";
pub const FLUSH_ENTRY: &str = "fs_blush_flush";
pub const GATE_ENTRY: &str = "fs_blush_gate";

/// The most eyes one face is charted with, pinned against `face.wgsl`'s
/// own `MAX_EYES`. Past this an eye simply is not painted; the array is
/// never read beyond its end.
pub const MAX_EYES: usize = 4;

/// A face plane's slot: full-extent `R32Float`.
///
/// Full-extent deliberately, and this is the one place in the graph where
/// that is a decision rather than a default. An iris is a couple of dozen
/// pixels across at the framing the engine is tuned for and its slit a
/// fraction of one, so the accents are the high-frequency content the
/// sheet carries — the reason the canvas stopped developing at half the
/// window's pixels in the first place.
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// The aperture's vertex layout: one clip-space corner, nothing else.
///
/// The loops arrive projected and fanned (see `face.wgsl`), so the vertex
/// stage has nothing to compute and the layout carries nothing to compute
/// it from. Keeping the projection on the CPU keeps it single-sourced
/// through [`regions::on_canvas`](crate::easel::regions::on_canvas),
/// which is the mapping the maps and the paint both have to agree with.
#[must_use]
pub fn geometry_slot() -> GeometrySlotSpec {
    GeometrySlotSpec { layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x2 }] }
}

/// Bytes one aperture vertex occupies.
pub const VERTEX_BYTES: usize = 8;

/// One eye's frame as the shader reads it — four four-wide lanes, so the
/// block's layout in the uniform address space is unambiguous.
pub struct EyeUniform {
    /// Where the iris centre landed on the canvas.
    pub centre: Vec2,
    /// The projected frame inverted: the two rows that take a canvas
    /// offset to iris coordinates.
    pub across: Vec2,
    pub down: Vec2,
    /// Pupil half-axes as fractions of the iris radius.
    pub pupil: Vec2,
    /// How far out from the centre the iris is measured, in canvas pixels.
    pub reach: f32,
    /// Zero once the two axes have collapsed onto one line and there is
    /// no basis left to invert — an eye seen exactly edge-on.
    pub valid: bool,
    /// Where this eye's cheek apple sits, and its radii.
    pub apple: Vec2,
    pub radii: Vec2,
    /// How much blush this eye has earned, already gated by how much of
    /// it the viewer can see.
    pub presence: f32,
}

impl EyeUniform {
    /// Bytes one eye occupies in the `FaceParams` array.
    pub const BYTES: usize = 64;

    fn encode(&self) -> [u8; Self::BYTES] {
        let mut bytes = [0u8; Self::BYTES];
        let lanes = [
            self.centre.x,
            self.centre.y,
            self.across.x,
            self.across.y,
            self.down.x,
            self.down.y,
            self.pupil.x,
            self.pupil.y,
            self.reach,
            f32::from(u8::from(self.valid)),
            self.apple.x,
            self.apple.y,
            self.radii.x,
            self.radii.y,
            self.presence,
            0.0,
        ];
        for (lane, value) in bytes.chunks_exact_mut(4).zip(lanes) {
            lane.copy_from_slice(&value.to_le_bytes());
        }

        bytes
    }
}

/// Every projected eye as the shader reads it, at the extent the eyes
/// were projected onto.
///
/// The wash develops the face twice over at two resolutions — the iris
/// and its lid at the sheet's own pixels, the cheek flush at the notched
/// body's — and every quantity in [`EyeUniform`] but the pupil fractions
/// and the presence is in canvas pixels. Rather than scale a frame from
/// one extent to the other, both callers project the chart's own frames
/// at their own extent through
/// [`accent::project`] and come here: one
/// statement of the packing, and no algebra that can slip a factor.
///
/// `presence` is indexed as `eyes` is — how much of each eye the viewer
/// can actually see, which is a world-space question and so the same
/// number at either extent.
#[must_use]
pub fn eyes(eyes: &[Eye], presence: &[f32], height: usize) -> Vec<EyeUniform> {
    let midline = accent::midline(eyes);

    eyes.iter()
        .enumerate()
        .map(|(index, eye)| {
            let (across, down) = eye.inverse().unwrap_or((Vec2::new(0.0, 0.0), Vec2::new(0.0, 0.0)));
            let (apple, radii) = accent::apple_of(eye, midline);

            EyeUniform {
                centre: eye.centre(),
                across,
                down,
                pupil: eye.pupil(),
                reach: eye.reach(height),
                valid: eye.inverse().is_some(),
                apple,
                radii,
                presence: presence.get(index).copied().unwrap_or(0.0),
            }
        })
        .collect()
}

/// Uniform window every face pass binds — the WGSL `FaceParams` block:
/// the eye count, three words of padding to the array's own alignment,
/// then [`MAX_EYES`] eyes whether or not the chart planted them.
pub struct FaceUniforms<'a> {
    pub eyes: &'a [EyeUniform],
}

impl FaceUniforms<'_> {
    /// The count word plus its padding, then the fixed-length array.
    pub const BYTES: u32 = 16 + (MAX_EYES * EyeUniform::BYTES) as u32;

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; Self::BYTES as usize];
        let count = self.eyes.len().min(MAX_EYES);
        bytes[0..4].copy_from_slice(&(count as u32).to_le_bytes());

        for (index, eye) in self.eyes.iter().take(MAX_EYES).enumerate() {
            let at = 16 + index * EyeUniform::BYTES;
            bytes[at..at + EyeUniform::BYTES].copy_from_slice(&eye.encode());
        }

        bytes
    }
}

/// One eye's aperture loop as a triangle fan about its own iris centre,
/// in clip space.
///
/// The loop is a lid over a lid — star-shaped about the iris centre by
/// construction, which is what makes a fan the whole fill rather than an
/// approximation of it. `width` and `height` are the canvas the loop was
/// projected onto.
fn fan(eye: &Eye, width: usize, height: usize, vertices: &mut Vec<u8>) {
    let loop_points = eye.aperture();
    if loop_points.len() < 3 {
        return;
    }

    let clip = |at: Vec2| [at.x / (width as f32 * 0.5) - 1.0, 1.0 - at.y / (height as f32 * 0.5)];
    let hub = clip(eye.centre());

    for (from, to) in loop_points.iter().zip(loop_points.iter().cycle().skip(1)).take(loop_points.len()) {
        for corner in [hub, clip(*from), clip(*to)] {
            for value in corner {
                vertices.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
}

/// Every eye's aperture, packed for [`geometry_slot`].
///
/// A develop has to fill its geometry slot whatever the chart planted — a
/// dispatch supplies one id per declared slot or it warn-drops whole — so
/// a face with no eyes neutralizes through the content, as the ink pass'
/// empty drawing does: one triangle collapsed to a point, which claims no
/// pixel.
#[must_use]
pub fn vertices(eyes: &[Eye], width: usize, height: usize) -> Vec<u8> {
    let mut packed = Vec::new();
    for eye in eyes {
        fan(eye, width, height, &mut packed);
    }
    if packed.is_empty() {
        packed = vec![0u8; 3 * VERTEX_BYTES];
    }

    packed
}

/// The aperture index buffer: sequential little-endian `u32` triangle-list
/// indices. Nothing is shared — every fan triangle carries its own hub.
#[must_use]
pub fn indices(eyes: &[Eye], width: usize, height: usize) -> Vec<u8> {
    let count = u32::try_from(vertices(eyes, width, height).len() / VERTEX_BYTES).unwrap_or(u32::MAX);

    (0..count).flat_map(u32::to_le_bytes).collect()
}

/// The aperture fill: one indexed draw of the bound fans, cleared first so
/// a develop never clips against the last one's lids.
#[must_use]
pub fn aperture_pass(geometry: u32, output: OutputSlot) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Draw(DrawPass {
            vertex_entry_point: APERTURE_VERTEX_ENTRY.to_owned(),
            geometry,
            depth: None,
            load: PassLoad::Clear,
        }),
        entry_point: APERTURE_ENTRY.to_owned(),
        inputs: Vec::new(),
        output,
        uniform_offset: 0,
        uniform_length: 0,
        repeat: None,
    }
}

/// The iris coverage, or the lid weight over it, off the softened clip.
#[must_use]
pub fn clipped_pass(entry_point: &str, clip: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    pass(entry_point, vec![clip], output, uniform_offset, FaceUniforms::BYTES)
}

/// The cheek flush, from the frames alone.
#[must_use]
pub fn flush_pass(output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    pass(FLUSH_ENTRY, Vec::new(), output, uniform_offset, FaceUniforms::BYTES)
}

/// The flush gated by the skin beneath it and the facing of that skin.
/// Every constant it reads is its own, so it windows nothing.
#[must_use]
pub fn gate_pass(flush: InputSlot, skin: InputSlot, packed: InputSlot, output: OutputSlot) -> ProgramPass {
    pass(GATE_ENTRY, vec![flush, skin, packed], output, 0, 0)
}

fn pass(
    entry_point: &str,
    inputs: Vec<InputSlot>,
    output: OutputSlot,
    uniform_offset: u32,
    uniform_length: u32,
) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: entry_point.to_owned(),
        inputs,
        output,
        uniform_offset,
        uniform_length,
        repeat: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_render::vertex_stride_bytes;

    /// Tripwire: the packer and the declared layout must agree on the
    /// stride. They are two independent statements of one byte
    /// arrangement, and a disagreement is not a compile error but a
    /// silently reinterpreted aperture — every corner sliding one lane.
    #[test]
    fn the_packed_aperture_vertex_matches_the_declared_stride() {
        assert_eq!(vertex_stride_bytes(&geometry_slot().layout), VERTEX_BYTES, "declared stride");
    }

    /// Tripwire: a face with no charted eye still has to produce a
    /// geometry a dispatch can name, or the develop warn-drops whole and
    /// the sheet silently stops updating.
    #[test]
    fn a_face_with_no_eyes_still_packs_one_triangle() {
        assert_eq!(vertices(&[], 120, 160).len(), 3 * VERTEX_BYTES);
        assert_eq!(indices(&[], 120, 160).len(), 3 * 4);
    }

    /// Tripwire: the uniform block's declared size must cover the count
    /// word, its padding, and every eye slot. A short block reads the
    /// next window's bytes as an eye frame, which paints an iris at a
    /// coordinate no chart ever planted.
    #[test]
    fn the_face_block_covers_every_eye_slot() {
        assert_eq!(FaceUniforms { eyes: &[] }.encode().len(), FaceUniforms::BYTES as usize);
    }
}
