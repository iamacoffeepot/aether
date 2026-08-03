//! The pigment ops as authored passes (iamacoffeepot/aether#4367).
//!
//! Where the pigment comes to rest: granulation against the uploaded
//! tooth plane, sag as the two downhill drag samples, spatter stamped
//! from the pre-rolled drops in the uniform blob, and the flow smear as
//! iterated advection passes over the flow field.
//!
//! The WGSL in [`PIGMENT_WGSL`] carries one fragment entry point per op,
//! each a texel-exact port of its CPU oracle — [`super::super::field`]'s
//! `granulate` / `sagged` / `spatter` and [`image::smear_along_flow`] —
//! and this module carries the typed builders around it: a pass-entry
//! function per op wiring entry point, input order and uniform window,
//! plus a uniform struct per op whose `encode` emits exactly the byte
//! layout its WGSL block declares. The coat sequencer
//! (iamacoffeepot/aether#4369) composes these into the wash program; the
//! parity scenarios in `tests/program_pigment_scenario.rs` drive each op
//! standalone against its oracle.
//!
//! Every plane an op touches is a [`plane_slot`]-shaped `R32Float`
//! texture — full f32 precision through the chain, replace semantics on
//! write (core WebGPU cannot blend 32-bit floats), nearest sampling
//! irrelevant because the ops read exact texels. Uniform windows carry
//! no alignment demands of their own (the dispatch path re-stages them
//! aligned), so a sequencer may pack op windows tight in one blob.

use core::f32::consts::{PI, TAU};

use aether_math::Vec2;
use aether_render::{InputSlot, OutputSlot, PassStage, ProgramPass, SlotExtent, SlotSpec, TextureFormat};

use crate::easel::field::DropAccident;
use crate::easel::image;

/// The WGSL module carrying the four pigment entry points
/// (`fs_granulate`, `fs_sag`, `fs_spatter`, `fs_smear`). Register it —
/// alone or concatenated with the other op modules' WGSL — and name
/// passes through the builders below.
pub const PIGMENT_WGSL: &str = include_str!("pigment.wgsl");

/// The declared shape every pigment plane shares: one `f32` per texel at
/// the canvas' own size. Density planes, the tooth, the flow components
/// and coherence all ride this spec, as dispatch bindings or transients
/// alike.
pub const fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// Uniform window for [`granulate_pass`] — the WGSL `GranulateParams`
/// block: one little-endian `f32`.
pub struct GranulateUniforms {
    /// How strongly the pigment settles into the tooth —
    /// `WashParams::gran`, the one granulation axis that varies per wash
    /// (floor, authority and pivot are constants shared with the oracle
    /// inside the WGSL).
    pub gran: f32,
}

impl GranulateUniforms {
    /// Bytes [`Self::encode`] emits — the WGSL block's size, and the
    /// window length [`granulate_pass`] declares.
    pub const BYTES: u32 = 4;

    pub fn encode(&self) -> Vec<u8> {
        self.gran.to_le_bytes().to_vec()
    }
}

/// The granulation pass: settle `density` into the paper's tooth —
/// field.rs `Sheet::granulate` as one pointwise pass. `density` and
/// `tooth` bind as inputs 0 and 1; the window at `uniform_offset` holds
/// an encoded [`GranulateUniforms`].
pub fn granulate_pass(density: InputSlot, tooth: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_granulate".to_owned(),
        inputs: vec![density, tooth],
        output,
        uniform_offset,
        uniform_length: GranulateUniforms::BYTES,
        repeat: None,
    }
}

/// The reference-sheet spacing of the two downhill drag samples —
/// mirrors field.rs `SAG_STEP`, private to the oracle.
const SAG_STEP_REFERENCE_PIXELS: f32 = 12.0;

/// Uniform window for [`sag_pass`] — the WGSL `SagParams` block: one
/// little-endian `u32`.
pub struct SagUniforms {
    /// Spacing of the downhill samples in whole texels, at least one.
    pub step_texels: u32,
}

impl SagUniforms {
    /// Bytes [`Self::encode`] emits — the WGSL block's size, and the
    /// window length [`sag_pass`] declares.
    pub const BYTES: u32 = 4;

    /// The step the CPU oracle takes at this canvas height: field.rs
    /// `SAG_STEP` through [`image::tuned`], rounded, floored at one
    /// texel — exactly `sagged`'s own derivation.
    pub fn for_canvas(height: usize) -> Self {
        Self { step_texels: image::tuned(SAG_STEP_REFERENCE_PIXELS, height).round().max(1.0) as u32 }
    }

    pub fn encode(&self) -> Vec<u8> {
        self.step_texels.to_le_bytes().to_vec()
    }
}

/// The sag pass: walk the softened puddle downhill — field.rs `sagged`
/// as one gather pass taking each of the two above-samples at its
/// strongest. `soft` binds as input 0; the window at `uniform_offset`
/// holds an encoded [`SagUniforms`].
pub fn sag_pass(soft: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_sag".to_owned(),
        inputs: vec![soft],
        output,
        uniform_offset,
        uniform_length: SagUniforms::BYTES,
        repeat: None,
    }
}

/// Ceiling on one spatter stamp's drop list — the WGSL uniform array's
/// fixed length. Generous against the rolled counts (the hair throws 20,
/// the atmosphere 12).
pub const MAX_SPATTER_DROPS: usize = 64;

/// Uniform window for [`spatter_pass`] — the WGSL `SpatterParams` block:
/// the centre pair, the live count, one pad word, then
/// [`MAX_SPATTER_DROPS`] four-float drop entries (bearing, throw,
/// radius, strength — [`DropAccident`] field order), zero-filled past
/// the live count.
pub struct SpatterUniforms<'a> {
    /// The region centroid the drops are thrown about, in texels — the
    /// same point the wash's `centroid` hands `Sheet::spatter`.
    pub centre: Vec2,
    /// The pre-rolled drops (#4372's `WashAccidents::drops`), at most
    /// [`MAX_SPATTER_DROPS`] of them.
    pub drops: &'a [DropAccident],
}

impl SpatterUniforms<'_> {
    /// Bytes [`Self::encode`] emits — the WGSL block's size, and the
    /// window length [`spatter_pass`] declares. The full fixed array is
    /// always present: the shader's declared block covers it, so the
    /// window must too.
    pub const BYTES: u32 = 16 + MAX_SPATTER_DROPS as u32 * 16;

    /// # Panics
    ///
    /// When more than [`MAX_SPATTER_DROPS`] drops are handed in — the
    /// fixed uniform array cannot carry them, and silently truncating
    /// the list would drop authored accidents.
    pub fn encode(&self) -> Vec<u8> {
        assert!(
            self.drops.len() <= MAX_SPATTER_DROPS,
            "spatter stamps at most {MAX_SPATTER_DROPS} drops; {} rolled",
            self.drops.len(),
        );

        let mut blob = Vec::with_capacity(Self::BYTES as usize);
        blob.extend_from_slice(&self.centre.x.to_le_bytes());
        blob.extend_from_slice(&self.centre.y.to_le_bytes());
        blob.extend_from_slice(&(self.drops.len() as u32).to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());

        for drop in self.drops {
            // The bearing rides wrapped into [-pi, pi]: WGSL's cos/sin
            // carry their specified accuracy only there, and the rolled
            // bearings live in [0, tau). Cosine and sine are periodic,
            // so the wrap moves the landing by under a millionth of a
            // texel per texel of throw.
            let bearing = drop.bearing.rem_euclid(TAU);
            let bearing = if bearing > PI {
                bearing - TAU
            } else {
                bearing
            };
            blob.extend_from_slice(&bearing.to_le_bytes());
            blob.extend_from_slice(&drop.throw.to_le_bytes());
            blob.extend_from_slice(&drop.radius.to_le_bytes());
            blob.extend_from_slice(&drop.strength.to_le_bytes());
        }

        blob.resize(Self::BYTES as usize, 0);
        blob
    }
}

/// The spatter pass: stamp the pre-rolled drops over `density` —
/// field.rs `Sheet::spatter` as one pass whose fragments walk the
/// bounded drop list instead of the oracle's per-drop bounding boxes.
/// `density` binds as input 0; the window at `uniform_offset` holds an
/// encoded [`SpatterUniforms`].
pub fn spatter_pass(density: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_spatter".to_owned(),
        inputs: vec![density],
        output,
        uniform_offset,
        uniform_length: SpatterUniforms::BYTES,
        repeat: None,
    }
}

/// Advection passes the smear runs — mirrors field.rs `SMEAR_PASSES`,
/// private to the oracle's `coats` call site.
pub const SMEAR_PASSES: u32 = 2;

/// The reference-sheet reach of one advection segment — mirrors
/// field.rs `SMEAR_REACH`, private to the oracle's `coats` call site.
const SMEAR_REACH_REFERENCE_PIXELS: f32 = 12.0;

/// Uniform window for the [`smear_passes`] chain — the WGSL
/// `SmearParams` block: one little-endian `i32`, shared by every pass in
/// the chain.
pub struct SmearUniforms {
    /// Steps taken either way along the local flow line, in texels.
    pub reach: i32,
}

impl SmearUniforms {
    /// Bytes [`Self::encode`] emits — the WGSL block's size, and the
    /// window length each [`smear_passes`] entry declares.
    pub const BYTES: u32 = 4;

    /// The reach the CPU call site uses at this canvas height: field.rs
    /// `SMEAR_REACH` through [`image::tuned`], rounded — exactly
    /// `Sheet::coats`' own derivation.
    pub fn for_canvas(height: usize) -> Self {
        Self { reach: image::tuned(SMEAR_REACH_REFERENCE_PIXELS, height).round() as i32 }
    }

    pub fn encode(&self) -> Vec<u8> {
        self.reach.to_le_bytes().to_vec()
    }
}

/// The planes one smear chain reads: the density it drags plus the flow
/// solved from the drawing ([`image::structure_tensor_flow`]'s three
/// components, uploaded as planes).
pub struct SmearSlots {
    /// The density plane the first pass advects.
    pub density: InputSlot,
    pub flow_x: InputSlot,
    pub flow_y: InputSlot,
    pub coherence: InputSlot,
}

/// The flow-smear chain: [`image::smear_along_flow`] as [`SMEAR_PASSES`]
/// advection passes, the first dragging `slots.density` into the
/// `scratch` transient and the second dragging that into `output` — the
/// oracle's `field = out` hand-off between passes, spoken as a
/// ping-pong. Every pass rereads the same flow planes and windows the
/// same encoded [`SmearUniforms`] at `uniform_offset`. `output` must not
/// resolve to the `scratch` transient — the second pass reads it.
pub fn smear_passes(
    slots: &SmearSlots,
    scratch: u32,
    output: OutputSlot,
    uniform_offset: u32,
) -> [ProgramPass; SMEAR_PASSES as usize] {
    let advect = |field: InputSlot, into: OutputSlot| ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_smear".to_owned(),
        inputs: vec![field, slots.flow_x, slots.flow_y, slots.coherence],
        output: into,
        uniform_offset,
        uniform_length: SmearUniforms::BYTES,
        repeat: None,
    };

    [
        advect(slots.density, OutputSlot::Transient { index: scratch }),
        advect(InputSlot::Transient { index: scratch }, output),
    ]
}
