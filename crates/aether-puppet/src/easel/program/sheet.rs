//! The composite ops as authored passes (iamacoffeepot/aether#4368).
//!
//! Where the coats become a sheet, re-spoken as WGSL over the authored
//! program surface (ADR-0170): the care-ramp mix of the tight and loose
//! develops (`field::material_wash`), the lost edge's directional
//! giveback about the region's centroid (`field::threshold`'s lost
//! branch), and the palette composite — per-material pigment over the
//! paper-shade plane into the final RGBA sheet ([`palette::composite`]).
//! The CPU code stays the oracle. The care field itself stays CPU-computed:
//! `field::care_field` runs on the class plane once per bake and
//! uploads as an ordinary plane, so only its application rides the GPU.
//!
//! The module is data plus builders, the shape the coat sequencer
//! (iamacoffeepot/aether#4369) composes: [`SHEET_WGSL`] carries the
//! entry points, each `*_pass` builder returns the `ProgramPass` that
//! invokes one of them with its inputs in the declared order, the slot
//! builders name the texture shapes those passes read and write, and
//! the params structs encode their uniform windows byte for byte
//! against the WGSL `SheetParams` block layout.
//!
//! Compositing runs as the CPU runs it: a light accumulator primed to
//! full transmission ([`light_prime_pass`]), one absorption per coat
//! ping-ponged between two [`sheet_slot`] transients
//! ([`coat_absorb_pass`]), then one resolve against paper white
//! ([`paper_composite_pass`]) into the sheet binding. Between coats the
//! accumulator lives in RGBA8, so a develop drifts from the CPU's `f32`
//! accumulator by up to one quantization step per coat — thresholded
//! parity, per ADR-0170, not bit-exact.

use aether_math::Vec2;
use aether_render::{InputSlot, OutputSlot, PassStage, ProgramPass, SlotExtent, SlotSpec, TextureFormat};

use super::super::palette;

/// The sheet ops' WGSL module, registered as (part of) a program's
/// `wgsl`. Fragment entry points only; the substrate owns the
/// fullscreen vertex stage.
pub const SHEET_WGSL: &str = include_str!("sheet.wgsl");

/// Size in bytes of the WGSL `SheetParams` uniform block: the window
/// length every windowed sheet pass declares, and the length both
/// params encoders produce. One shared block rather than one per op so
/// the module carries a single `@group(0) @binding(0)` declaration;
/// each entry point reads only its own fields.
pub const SHEET_PARAMS_BYTES: u32 = 32;

/// A data plane slot: one `f32` per texel at the sheet's own size — the
/// shape of every density, care, and paper-shade plane the sheet passes
/// read, and of the alpha plane the lost edge writes.
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// An RGBA8 slot at the sheet's own size: the sheet binding the
/// composite resolves into, and the pair of light-accumulator
/// transients the absorption passes ping-pong between.
pub fn sheet_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }
}

/// Uniform window for [`lost_edge_pass`]: which way one wash gives up
/// its edge (`WashParams::losing`), and about where.
pub struct LostEdgeParams {
    /// The region's centroid in sheet texel coordinates — the pole the
    /// giveback's bearings are measured about (`field::centroid`).
    pub centre: Vec2,
    /// Direction of the side that dissolves into the paper, in radians
    /// about `centre` (`WashParams::lost`).
    pub angle: f32,
}

impl LostEdgeParams {
    /// This op's `SheetParams` window: `centre` at bytes 0..8, `angle`
    /// at 8..12, the coat fields zeroed.
    pub fn encode(&self) -> [u8; SHEET_PARAMS_BYTES as usize] {
        let mut window = [0u8; SHEET_PARAMS_BYTES as usize];
        window[0..4].copy_from_slice(&self.centre.x.to_le_bytes());
        window[4..8].copy_from_slice(&self.centre.y.to_le_bytes());
        window[8..12].copy_from_slice(&self.angle.to_le_bytes());
        window
    }
}

/// Uniform window for [`coat_absorb_pass`]: one
/// [`Coat`](palette::Coat)'s pigment and cap.
pub struct CoatParams {
    /// Packed `0xRRGGBB`, the form the palette authors — `encode`
    /// unpacks it through [`palette::channels`], so the pigment floor
    /// lands here exactly as it lands in the CPU composite.
    pub pigment: u32,
    /// Ceiling on the coat's deposit ([`Coat::cap`](palette::Coat::cap)).
    pub cap: f32,
}

impl CoatParams {
    /// This op's `SheetParams` window: `cap` at bytes 12..16, the
    /// pigment's three transmissions at 16..28 (28..32 is the `vec4`
    /// padding lane), the lost-edge fields zeroed.
    pub fn encode(&self) -> [u8; SHEET_PARAMS_BYTES as usize] {
        let mut window = [0u8; SHEET_PARAMS_BYTES as usize];
        window[12..16].copy_from_slice(&self.cap.to_le_bytes());
        for (lane, channel) in window[16..28].chunks_exact_mut(4).zip(palette::channels(self.pigment)) {
            lane.copy_from_slice(&channel.to_le_bytes());
        }
        window
    }
}

/// The care ramp applied (`fs_care_mix`): `tight` held wherever the
/// `care` plane is one, `loose` freed wherever it is zero, mixed on the
/// ramp between. All three inputs and the output are [`plane_slot`]
/// shapes. Uniform-less — the mix has no axes of its own.
pub fn care_mix_pass(tight: InputSlot, loose: InputSlot, care: InputSlot, output: OutputSlot) -> ProgramPass {
    pass("fs_care_mix", vec![tight, loose, care], output, 0, 0)
}

/// The lost edge (`fs_lost_edge`): `hard` is the thresholded alpha,
/// `soft` the softened puddle it was cut from, both [`plane_slot`]
/// shapes, and the output is the alpha with one side given up.
/// `uniform_offset` locates a [`LostEdgeParams::encode`] window in the
/// dispatch blob.
pub fn lost_edge_pass(hard: InputSlot, soft: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    pass("fs_lost_edge", vec![hard, soft], output, uniform_offset, SHEET_PARAMS_BYTES)
}

/// The unpainted sheet (`fs_light_prime`): full transmission written
/// into a [`sheet_slot`] light accumulator, the empty product the
/// absorption chain multiplies down. No inputs, no uniforms.
pub fn light_prime_pass(output: OutputSlot) -> ProgramPass {
    pass("fs_light_prime", Vec::new(), output, 0, 0)
}

/// One coat's absorption (`fs_coat_absorb`): `light` is the accumulator
/// so far (a [`sheet_slot`] shape), `density` the coat's
/// [`plane_slot`] plane, and the output the next accumulator hop —
/// ping-pong two transients, one hop per coat. `uniform_offset`
/// locates a [`CoatParams::encode`] window in the dispatch blob.
pub fn coat_absorb_pass(light: InputSlot, density: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    pass("fs_coat_absorb", vec![light, density], output, uniform_offset, SHEET_PARAMS_BYTES)
}

/// The resolve against paper white (`fs_paper_composite`): `light` is
/// the fully absorbed accumulator, `paper_shade` the sheet's own tooth
/// and mottle as a multiplier about one (`Sheet::paper_shade`, a
/// [`plane_slot`] shape), and the output the finished RGBA sheet —
/// opaque everywhere, the alpha-255 convention the easel billboard
/// depends on. Uniform-less: the paper's colour is a constant of the
/// medium, baked into the WGSL as it is baked into the palette.
pub fn paper_composite_pass(light: InputSlot, paper_shade: InputSlot, output: OutputSlot) -> ProgramPass {
    pass("fs_paper_composite", vec![light, paper_shade], output, 0, 0)
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
