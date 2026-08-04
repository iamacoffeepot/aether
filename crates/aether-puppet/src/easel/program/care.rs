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

use aether_render::{InputSlot, OutputSlot, PassStage, ProgramPass, SlotExtent, SlotSpec, TextureFormat};

use crate::easel::field::{CARE_FAR, CARE_NEAR};
use crate::easel::image;

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
}

impl RampUniforms {
    pub const BYTES: u32 = 8;

    /// The ramp resolved for one canvas, through the same
    /// [`image::tuned`] the CPU field converts with.
    pub fn for_canvas(height: usize) -> Self {
        Self { far: image::tuned(CARE_FAR, height), near: image::tuned(CARE_NEAR, height) }
    }

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        bytes[0..4].copy_from_slice(&self.far.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.near.to_le_bytes());
        bytes
    }
}

/// Total uniform bytes one [`passes`] chain windows: one hop window each,
/// then the ramp's.
pub const UNIFORM_BYTES: u32 = HOPS.len() as u32 * JumpUniforms::BYTES + RampUniforms::BYTES;

/// The whole transform as passes: the seed, [`HOPS`] flood hops
/// ping-ponging between two planes, and the ramp into `output`.
///
/// `classes` is the packed bake plane — the flood reads its class
/// channel. `carry` and `relay` are indices of two [`plane_slot`]
/// transients the flood ping-pongs between; the caller declares them,
/// distinct from each other and from `output`. The chain's uniform windows encode at `uniform_offset`
/// through [`encode`].
pub fn passes(classes: InputSlot, carry: u32, relay: u32, output: OutputSlot, uniform_offset: u32) -> Vec<ProgramPass> {
    let mut passes = Vec::with_capacity(HOPS.len() + 2);
    passes.push(pass(SEED_ENTRY, classes, OutputSlot::Transient { index: carry }, uniform_offset, 0));

    let (mut read, mut write) = (carry, relay);
    for hop in 0..HOPS.len() as u32 {
        passes.push(pass(
            JUMP_ENTRY,
            InputSlot::Transient { index: read },
            OutputSlot::Transient { index: write },
            uniform_offset + hop * JumpUniforms::BYTES,
            JumpUniforms::BYTES,
        ));
        (read, write) = (write, read);
    }

    passes.push(pass(
        RAMP_ENTRY,
        InputSlot::Transient { index: read },
        output,
        uniform_offset + HOPS.len() as u32 * JumpUniforms::BYTES,
        RampUniforms::BYTES,
    ));

    passes
}

/// Write the chain's windows: the hop schedule and the canvas' own ramp.
///
/// Every value here is fixed by the schedule and the canvas height, so
/// this belongs to the develop's static uniform slice — nothing in it
/// turns with the view.
pub fn encode(blob: &mut [u8], uniform_offset: u32, height: usize) {
    let at = uniform_offset as usize;
    for (hop, &step) in HOPS.iter().enumerate() {
        let window = at + hop * JumpUniforms::BYTES as usize;
        blob[window..window + JumpUniforms::BYTES as usize].copy_from_slice(&JumpUniforms { step }.encode());
    }

    let ramp = at + HOPS.len() * JumpUniforms::BYTES as usize;
    blob[ramp..ramp + RampUniforms::BYTES as usize].copy_from_slice(&RampUniforms::for_canvas(height).encode());
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
