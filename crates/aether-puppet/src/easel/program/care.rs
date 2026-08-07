//! The care field as authored passes (iamacoffeepot/aether#4387,
//! ADR-0171): a jump-flood distance transform over the packed bake's
//! class channel, ramped into how closely the hand is held.
//!
//! [`field::care_field`](crate::easel::field::care_field) stays the CPU
//! oracle and keeps its chamfer sweeps; `care.wgsl`'s own commentary
//! carries the argument for why the GPU floods instead and what the two
//! answers cost each other. This module owns the Rust half of the
//! contract: the hop schedule, the uniform windows, and the pass builder
//! that lays the whole transform into a graph.

use aether_math::Vec2;
use aether_render::{InputSlot, OutputSlot, PassStage, ProgramPass, SlotExtent, SlotSpec, TextureFormat};

use crate::easel::field::{CARE_FAR, CARE_NEAR, CareSource};
use crate::easel::image;
use crate::easel::palette;

/// The care WGSL. Never registered alone — the wash program's
/// [`module`](super::wash::module) concatenates it with the op modules
/// whose `hermite` it calls.
pub const CARE_WGSL: &str = include_str!("care.wgsl");

/// Entry point seeding the flood from the class channel.
pub const SEED_ENTRY: &str = "fs_care_seed";

/// Entry point of one flood hop.
pub const JUMP_ENTRY: &str = "fs_care_jump";

/// Entry point ramping the flooded seeds into the care field.
pub const RAMP_ENTRY: &str = "fs_care_ramp";

/// The hop schedule: halving reaches from past any canvas edge down to
/// one, then a second hop at one.
///
/// The schedule is fixed rather than derived from the canvas because the
/// graph is static by construction — the same passes for every subject at
/// every size. Starting past the widest canvas the easel paints at
/// (`easel::CANVAS_LONG_EDGE`) costs a few
/// hops whose probes all clamp onto the border and find nothing new; it
/// is what lets one schedule serve every size. The repeated final hop is
/// the standard jump-flood refinement: the algorithm is approximate, and
/// almost every error it leaves is corrected by one more pass at reach
/// one.
pub const HOPS: &[i32] = &[2048, 1024, 512, 256, 128, 64, 32, 16, 8, 4, 2, 1, 1];

/// The plane the flood carries its seeds on, and the plane it ramps into:
/// full-extent `R32Float`, so a texel index survives as the integer it is.
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// Uniform window for the seed — the WGSL `CareSeedParams` block.
pub struct SeedUniforms {
    /// The drawn features as a class bit set
    /// ([`Palette::face_classes`](crate::easel::palette::Palette::face_classes)
    /// through [`class_set`](crate::easel::palette::class_set)).
    pub features: u32,
    /// Which arm the develop resolved to, per [`CareSource::arm`].
    pub source: u32,
    /// The authored focus in this plane's own texels.
    pub anchor: Vec2,
}

impl SeedUniforms {
    pub const BYTES: u32 = 16;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        bytes[0..4].copy_from_slice(&self.features.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.source.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.anchor.x.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.anchor.y.to_le_bytes());
        bytes
    }
}

/// Uniform window for one hop — the WGSL `CareJumpParams` block.
pub struct JumpUniforms {
    /// How far this hop reaches, in texels.
    pub step: i32,
}

impl JumpUniforms {
    pub const BYTES: u32 = 4;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        self.step.to_le_bytes()
    }
}

/// Uniform window for the ramp — the WGSL `CareRampParams` block.
pub struct RampUniforms {
    /// `CARE_FAR` and `CARE_NEAR` at this canvas' own height.
    pub far: f32,
    pub near: f32,
    /// Which arm the develop resolved to, per [`CareSource::arm`]. The
    /// unanchored one has no distance to ramp and short-circuits.
    pub source: u32,
}

impl RampUniforms {
    pub const BYTES: u32 = 12;

    /// The ramp resolved for one canvas, through the same
    /// [`image::tuned`] the CPU field converts with.
    pub fn for_canvas(height: usize, source: u32) -> Self {
        Self { far: image::tuned(CARE_FAR, height), near: image::tuned(CARE_NEAR, height), source }
    }

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        bytes[0..4].copy_from_slice(&self.far.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.near.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.source.to_le_bytes());
        bytes
    }
}

/// Total uniform bytes one [`passes`] chain windows: the seed's, one hop
/// window each, then the ramp's.
pub const UNIFORM_BYTES: u32 = SeedUniforms::BYTES + HOPS.len() as u32 * JumpUniforms::BYTES + RampUniforms::BYTES;

/// Where the hop windows begin, past the seed's.
const HOPS_AT: u32 = SeedUniforms::BYTES;

/// The whole transform as passes: the seed, [`HOPS`] flood hops
/// ping-ponging between two planes, and the ramp into `output`.
///
/// `classes` is the packed bake plane — the flood reads its class
/// channel. `carry` and `relay` are indices of two [`plane_slot`]
/// transients the flood ping-pongs between; the caller declares them,
/// distinct from each other and from `output`. The chain's uniform windows encode at `uniform_offset`
/// through [`encode_hops`] and [`encode_frame`].
pub fn passes(classes: InputSlot, carry: u32, relay: u32, output: OutputSlot, uniform_offset: u32) -> Vec<ProgramPass> {
    let mut passes = Vec::with_capacity(HOPS.len() + 2);
    passes.push(pass(SEED_ENTRY, classes, OutputSlot::Transient { index: carry }, uniform_offset, SeedUniforms::BYTES));

    let (mut read, mut write) = (carry, relay);
    for hop in 0..HOPS.len() as u32 {
        passes.push(pass(
            JUMP_ENTRY,
            InputSlot::Transient { index: read },
            OutputSlot::Transient { index: write },
            uniform_offset + HOPS_AT + hop * JumpUniforms::BYTES,
            JumpUniforms::BYTES,
        ));
        (read, write) = (write, read);
    }

    passes.push(pass(
        RAMP_ENTRY,
        InputSlot::Transient { index: read },
        output,
        uniform_offset + HOPS_AT + HOPS.len() as u32 * JumpUniforms::BYTES,
        RampUniforms::BYTES,
    ));

    passes
}

/// Write the hop schedule, which nothing but the schedule decides.
pub fn encode_hops(blob: &mut [u8], uniform_offset: u32) {
    let hops_at = uniform_offset as usize + HOPS_AT as usize;
    for (hop, &step) in HOPS.iter().enumerate() {
        let window = hops_at + hop * JumpUniforms::BYTES as usize;
        blob[window..window + JumpUniforms::BYTES as usize].copy_from_slice(&JumpUniforms { step }.encode());
    }
}

/// Write the two windows that read the resolved source: the seed and the
/// ramp.
///
/// Both per frame, and the ramp is the reason. The authored focus is a
/// point in the subject's own space, so a view can fail to place it at
/// all — and when it does, the source falls through to the level hand
/// ([`CareSource::resolve`]). A ramp fixed per subject would still be on
/// the anchor arm for that frame and would resolve the empty flood to a
/// wholly free hand instead, so the CPU oracle and this would disagree on
/// exactly the frames the fall-through exists for.
pub fn encode_frame(blob: &mut [u8], uniform_offset: u32, height: usize, source: &CareSource) {
    let (features, anchor) = match source {
        CareSource::Features(classes) => (palette::class_set(classes), Vec2::new(0.0, 0.0)),
        CareSource::Anchor(at) => (0, *at),
        CareSource::Even => (0, Vec2::new(0.0, 0.0)),
    };

    let at = uniform_offset as usize;
    let seeded = SeedUniforms { features, source: source.arm(), anchor };
    blob[at..at + SeedUniforms::BYTES as usize].copy_from_slice(&seeded.encode());

    let ramp = at + HOPS_AT as usize + HOPS.len() * JumpUniforms::BYTES as usize;
    let resolved = RampUniforms::for_canvas(height, source.arm());
    blob[ramp..ramp + RampUniforms::BYTES as usize].copy_from_slice(&resolved.encode());
}

fn pass(
    entry_point: &str,
    input: InputSlot,
    output: OutputSlot,
    uniform_offset: u32,
    uniform_length: u32,
) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: entry_point.to_owned(),
        inputs: vec![input],
        output,
        uniform_offset,
        uniform_length,
        repeat: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::easel::CANVAS_LONG_EDGE;

    /// Tripwire: the hop schedule must reach past the widest canvas the
    /// easel paints at, and must end at one.
    ///
    /// A schedule that starts short leaves every pixel further than its
    /// first hop from a feature reporting the unreached sentinel, which
    /// ramps to zero care — the whole picture painted with a free hand
    /// and nothing erroring. A schedule that does not end at one leaves
    /// the flood quantized to its last reach.
    #[test]
    fn the_hop_schedule_covers_the_widest_canvas_and_lands_on_one() {
        assert!(HOPS[0] >= CANVAS_LONG_EDGE as i32, "the first hop must reach past the widest canvas, got {}", HOPS[0]);
        assert_eq!(HOPS.last().copied(), Some(1), "the flood must finish at reach one");

        for pair in HOPS.windows(2) {
            assert!(pair[1] == pair[0] / 2 || pair[1] == pair[0], "hops halve or repeat, got {pair:?}");
        }
    }
}
