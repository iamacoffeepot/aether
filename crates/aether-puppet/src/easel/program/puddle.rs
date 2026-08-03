//! The puddle ops as authored passes (iamacoffeepot/aether#4366).
//!
//! Where the water decides the edge: the separable box blur (iterated
//! small-tap, held against the CPU running sum within a stated similarity
//! threshold rather than bit-exactly), the shrink that resamples a pour
//! about its centroid with the pre-rolled jitter, the threshold that cuts
//! the softened puddle along a window of the tide-line noise, and the rim
//! — alpha minus its own blur, noise-varied along the tide line.
//!
//! The WGSL lives in [`PUDDLE_WGSL`] and every formula in it transcribes
//! its CPU counterpart in [`super::super::image`] / [`super::super::field`]
//! (see `puddle.wgsl`'s own commentary for the op-by-op mapping). This
//! module owns the Rust side of the contract: the entry-point names, the
//! pass builders that lay each op into an ADR-0170 program graph, and the
//! uniform structs whose `encode` produces bytes in exactly the layout the
//! shader's uniform blocks declare — all-scalar structs, so the layout is
//! four bytes per field in declaration order with no padding. The coat
//! sequencer (iamacoffeepot/aether#4369) composes these into the wash's
//! full pass graph; the parity scenarios in
//! `tests/program_puddle_scenario.rs` drive each op standalone.
//!
//! Ops read and write [`plane_slot`] data planes (`R32Float`, so an
//! intermediate never quantizes); a chain's final pass may instead target
//! an `Rgba8` writable binding, which quantizes the plane to 8 bits at the
//! very end — the form the parity scenarios observe through the overlay
//! path.

use aether_math::Vec2;
use aether_render::{InputSlot, OutputSlot, PassStage, ProgramPass, SlotExtent, SlotSpec, TextureFormat};

/// The puddle WGSL module: one fragment entry point per op, registered
/// (usually alongside sibling op modules, concatenated) through
/// `aether.render.program.register`.
pub const PUDDLE_WGSL: &str = include_str!("puddle.wgsl");

/// Entry point of one separable box-average sweep ([`box_blur_passes`]).
pub const BOX_BLUR_ENTRY: &str = "fs_box_blur";

/// Entry point of the scale-about-centroid resample ([`shrink_pass`]).
pub const SHRINK_ENTRY: &str = "fs_shrink";

/// Entry point of the noise-windowed threshold band ([`threshold_pass`]).
pub const THRESHOLD_ENTRY: &str = "fs_threshold";

/// Entry point of the tide-line rim ([`rim_pass`]).
pub const RIM_ENTRY: &str = "fs_rim";

/// How much narrower one box pass is than the Gaussian three of them
/// stand in for. Mirrors the private constant in `easel::image` — the
/// blur parity scenario pins the two against each other.
pub const BOX_TO_GAUSSIAN: f32 = 1.7;

/// Box iterations per blur, matching `easel::image`'s pass count: three
/// is where the corners stop showing. Each iteration is two program
/// passes (horizontal, then vertical), so [`box_blur_passes`] emits six.
pub const BLUR_ITERATIONS: u32 = 3;

/// Half-width of the band the puddle is thresholded across. Mirrors the
/// private constant in `easel::field`; callers put it in
/// [`ThresholdUniforms::band`] unless a wash deliberately widens it.
pub const EDGE_BAND: f32 = 0.08;

/// Middle strength of the tide line and how far the edge noise swings it
/// either side. Mirrors `easel::field`'s private pair;
/// [`RimUniforms::encode`] bakes it into the rim window.
pub const RIM_VARY: (f32, f32) = (0.55, 1.5);

/// Ceiling on the darkest the tide line may go. Mirrors `easel::field`.
pub const RIM_VARY_CEILING: f32 = 1.3;

/// How far the rim's window into the noise is displaced past the one
/// that placed the edge, in multiples of the pour's own offset. Mirrors
/// `easel::field`; [`RimUniforms::encode`] applies it, so the rim's
/// strength varies along a different stretch of noise than the signal
/// that decided where the tide line went.
pub const RIM_RESTRIDE: (u32, u32) = (3, 7);

/// How much stronger the rim reads than the body it edges. Mirrors
/// `easel::field`; the sequencer folds it into [`RimUniforms::strength`]
/// as `params.rim * params.load * RIM_GAIN`.
pub const RIM_GAIN: f32 = 2.2;

/// The data-plane slot every puddle op reads and writes: full-extent
/// `R32Float`, nearest-bound (ADR-0170), so plane values survive a chain
/// of passes without quantizing and without filtering inventing values
/// no texel holds.
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// The box radius one `image::blur` call uses for a softening of
/// `radius_pixels` (already converted to this sheet's pixels): the same
/// `(radius / BOX_TO_GAUSSIAN).round()` mapping as the CPU, computed in
/// `f32` so both sides round identically. Zero means the CPU blur is a
/// no-op; a zero-radius chain degenerates to six copies, so callers
/// should skip the chain instead.
pub fn box_radius_texels(radius_pixels: f32) -> u32 {
    (radius_pixels / BOX_TO_GAUSSIAN).round().max(0.0) as u32
}

/// Uniforms for one blur chain: both sweep windows of
/// [`box_blur_passes`], horizontal then vertical, each an all-`i32`
/// `BoxBlurParams` block (`axis_x`, `axis_y`, `radius_texels`).
pub struct BoxBlurUniforms {
    /// Window half-width in texels, from [`box_radius_texels`].
    pub radius_texels: u32,
}

/// Bytes of one `BoxBlurParams` uniform window.
const BOX_BLUR_WINDOW_BYTES: u32 = 12;

impl BoxBlurUniforms {
    /// Total bytes [`Self::encode`] appends at the chain's
    /// `uniform_offset`: the horizontal window then the vertical one.
    pub const BYTES: u32 = 2 * BOX_BLUR_WINDOW_BYTES;

    /// The two windows, packed tight in the blob layout the shader
    /// declares.
    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        encode_words([
            1u32.to_le_bytes(),
            0u32.to_le_bytes(),
            self.radius_texels.to_le_bytes(),
            0u32.to_le_bytes(),
            1u32.to_le_bytes(),
            self.radius_texels.to_le_bytes(),
        ])
    }
}

/// The full blur chain mirroring `image::blur`: [`BLUR_ITERATIONS`] box
/// iterations, each a horizontal sweep into the `scratch` transient and a
/// vertical sweep out of it — the vertical result parking in `carry`
/// between iterations and landing in `output` on the last. `scratch` and
/// `carry` are indices of two [`plane_slot`] transients the caller
/// declares (distinct, and distinct from `output` when that is a
/// transient); the chain's [`BoxBlurUniforms`] encode at
/// `uniform_offset`, shared by every iteration.
pub fn box_blur_passes(
    source: InputSlot,
    scratch: u32,
    carry: u32,
    output: OutputSlot,
    uniform_offset: u32,
) -> Vec<ProgramPass> {
    let mut passes = Vec::with_capacity(2 * BLUR_ITERATIONS as usize);
    let mut read = source;

    for iteration in 0..BLUR_ITERATIONS {
        let vertical_out = if iteration + 1 == BLUR_ITERATIONS {
            output
        } else {
            OutputSlot::Transient { index: carry }
        };
        passes.push(pass(
            BOX_BLUR_ENTRY,
            vec![read],
            OutputSlot::Transient { index: scratch },
            uniform_offset,
            BOX_BLUR_WINDOW_BYTES,
        ));
        passes.push(pass(
            BOX_BLUR_ENTRY,
            vec![InputSlot::Transient { index: scratch }],
            vertical_out,
            uniform_offset + BOX_BLUR_WINDOW_BYTES,
            BOX_BLUR_WINDOW_BYTES,
        ));
        read = InputSlot::Transient { index: carry };
    }

    passes
}

/// Uniforms for [`shrink_pass`]: the `ShrinkParams` block, five `f32`s
/// in declaration order.
pub struct ShrinkUniforms {
    /// The wash's centroid, the fixed point of the resample.
    pub centre: Vec2,
    /// The pour's pre-rolled wander off that centre, in this sheet's
    /// pixels (`PourAccident::jitter`).
    pub jitter: Vec2,
    /// Size relative to the region (`Pour::scale`); below one the touch
    /// lands inside the last.
    pub scale: f32,
}

impl ShrinkUniforms {
    /// Bytes [`Self::encode`] appends at the pass's `uniform_offset`.
    pub const BYTES: u32 = 20;

    /// The window bytes in the blob layout the shader declares.
    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        encode_words([
            self.centre.x.to_le_bytes(),
            self.centre.y.to_le_bytes(),
            self.jitter.x.to_le_bytes(),
            self.jitter.y.to_le_bytes(),
            self.scale.to_le_bytes(),
        ])
    }
}

/// The scale-about-centroid resample (`field::shrink`): one pass reading
/// the region mask plane and writing the placed pour.
pub fn shrink_pass(source: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    pass(SHRINK_ENTRY, vec![source], output, uniform_offset, ShrinkUniforms::BYTES)
}

/// Uniforms for [`threshold_pass`]: the `ThresholdParams` block — the
/// noise window pair then `level`, `band`, `wobble`.
pub struct ThresholdUniforms {
    /// Which window of the tide-line noise decides this pour's edge
    /// (`PourAccident::window`): texel offsets the shader wraps into the
    /// noise plane.
    pub window: (u32, u32),
    /// Where in the softened puddle the edge is taken
    /// (`WashParams::level`).
    pub level: f32,
    /// Half-width of the threshold band, [`EDGE_BAND`] unless a wash
    /// deliberately widens it.
    pub band: f32,
    /// How far the tide-line noise moves the edge (`WashParams::wobble`).
    pub wobble: f32,
}

impl ThresholdUniforms {
    /// Bytes [`Self::encode`] appends at the pass's `uniform_offset`.
    pub const BYTES: u32 = 20;

    /// The window bytes in the blob layout the shader declares.
    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        encode_words([
            self.window.0.to_le_bytes(),
            self.window.1.to_le_bytes(),
            self.level.to_le_bytes(),
            self.band.to_le_bytes(),
            self.wobble.to_le_bytes(),
        ])
    }
}

/// The noise-windowed threshold band (`Sheet::threshold`'s hard edge):
/// one pass reading the softened puddle and the shared edge-noise plane.
/// The lost-edge giveback belongs to the composite slice
/// (iamacoffeepot/aether#4368), not this pass.
pub fn threshold_pass(soft: InputSlot, edge_noise: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    pass(THRESHOLD_ENTRY, vec![soft, edge_noise], output, uniform_offset, ThresholdUniforms::BYTES)
}

/// Uniforms for [`rim_pass`]: the `RimParams` block. [`Self::encode`]
/// derives the shader's six words from the two fields here — the noise
/// offsets as the pour's window restrided by [`RIM_RESTRIDE`], then
/// [`RIM_VARY`], [`RIM_VARY_CEILING`], and `strength`.
pub struct RimUniforms {
    /// The pour's noise window — the same pair its threshold read; the
    /// restride displaces the rim's read past it.
    pub window: (u32, u32),
    /// The folded rim multiplier: the pour's `params.rim * params.load`
    /// times [`RIM_GAIN`].
    pub strength: f32,
}

impl RimUniforms {
    /// Bytes [`Self::encode`] appends at the pass's `uniform_offset`.
    pub const BYTES: u32 = 24;

    /// The window bytes in the blob layout the shader declares.
    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        encode_words([
            (self.window.0 * RIM_RESTRIDE.0).to_le_bytes(),
            (self.window.1 * RIM_RESTRIDE.1).to_le_bytes(),
            RIM_VARY.0.to_le_bytes(),
            RIM_VARY.1.to_le_bytes(),
            RIM_VARY_CEILING.to_le_bytes(),
            self.strength.to_le_bytes(),
        ])
    }
}

/// The tide-line rim (the rim block inside `Sheet::pour`): one pass over
/// the thresholded alpha, its blurred interior (from a [`box_blur_passes`]
/// chain at the wash's rim radius), and the shared edge-noise plane.
pub fn rim_pass(
    alpha: InputSlot,
    interior: InputSlot,
    edge_noise: InputSlot,
    output: OutputSlot,
    uniform_offset: u32,
) -> ProgramPass {
    pass(RIM_ENTRY, vec![alpha, interior, edge_noise], output, uniform_offset, RimUniforms::BYTES)
}

/// Pack four-byte words into a tight little-endian blob — the layout
/// every all-scalar uniform block here declares.
fn encode_words<const WORDS: usize, const BYTES: usize>(words: [[u8; 4]; WORDS]) -> [u8; BYTES] {
    const {
        assert!(4 * WORDS == BYTES, "an encode's declared BYTES must cover its words exactly");
    }

    let mut bytes = [0u8; BYTES];
    for (slot, word) in bytes.chunks_exact_mut(4).zip(words) {
        slot.copy_from_slice(&word);
    }
    bytes
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
