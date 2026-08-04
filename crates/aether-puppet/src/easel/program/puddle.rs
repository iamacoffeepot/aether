//! The puddle ops as authored passes (iamacoffeepot/aether#4366).
//!
//! Where the water decides the edge: the separable box blur (iterated
//! small-tap, held against the CPU running sum within a stated similarity
//! threshold rather than bit-exactly, and swept at a reduced extent when
//! its radius is wide enough to spare the texels), the shrink that
//! resamples a pour
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

/// Entry points of a reduced-extent chain's two ends: the block average
/// into the reduced plane and the bilinear carry back out of it.
pub const BOX_DOWNSAMPLE_ENTRY: &str = "fs_box_downsample";
pub const BOX_UPSAMPLE_ENTRY: &str = "fs_box_upsample";

/// Entry point of the scale-about-centroid resample ([`shrink_pass`]).
pub const SHRINK_ENTRY: &str = "fs_shrink";

/// Entry point of the noise-windowed threshold band ([`threshold_pass`]).
pub const THRESHOLD_ENTRY: &str = "fs_threshold";

/// Entry point of the tide-line rim ([`rim_pass`]).
pub const RIM_ENTRY: &str = "fs_rim";

/// The one source of these is the CPU wash itself: [`ThresholdUniforms`]
/// and [`RimUniforms`] window the same [`EDGE_BAND`] / [`RIM_VARY`] /
/// [`RIM_RESTRIDE`] the oracle thresholds and rims with, the sequencer
/// folds [`RIM_GAIN`] into [`RimUniforms::strength`] as
/// `params.rim * params.load * RIM_GAIN`, and [`box_radius_texels`] and
/// [`box_blur_passes`] round and iterate through the same
/// [`BOX_TO_GAUSSIAN`] / [`BLUR_PASSES`] as `image::blur` — each blur is
/// `2 * BLUR_PASSES` sweep passes, horizontal then vertical, plus the
/// two ends a reduced-extent chain adds.
pub use crate::easel::field::{EDGE_BAND, RIM_GAIN, RIM_RESTRIDE, RIM_VARY, RIM_VARY_CEILING};
pub use crate::easel::image::{BLUR_PASSES, BOX_TO_GAUSSIAN};

/// The data-plane slot every puddle op reads and writes: full-extent
/// `R32Float`, nearest-bound (ADR-0170), so plane values survive a chain
/// of passes without quantizing and without filtering inventing values
/// no texel holds.
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// The plane a blur chain of [`BoxBlurChain::divisor`] sweeps over: the
/// data plane at a `divisor`-th of the canvas on each axis. A divisor of
/// one is [`plane_slot`] itself — the executor pools both onto the same
/// texture (ADR-0170), so a full-extent chain declared this way allocates
/// nothing extra.
pub fn reduced_plane_slot(divisor: u32) -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Divided { divisor } }
}

/// Extents a blur chain may run reduced at, widest reduction first. Two
/// and four keep the block average a whole number of texels on each axis,
/// which is what lets the downsample be an exact box average rather than
/// a resample.
const REDUCED_DIVISORS: [u32; 2] = [4, 2];

/// Narrowest window, in texels of its own reduced plane, a chain may
/// sweep once reduced. What the reduction costs is set by how many
/// samples the window spans on the plane it is swept over, not by any
/// absolute size — the block average and the bilinear carry-back
/// contribute their own support to the softening, and the carry-back
/// resolves the reduced plane's curve as straight lines between its
/// samples — so the bound holds the same at every divisor. Two and a
/// half texels is where the window carries whole interior taps either
/// side of its centre rather than resting on the straddling pair, and
/// puts the reduction's own support around a twentieth of the softening's
/// spread. It also decides where a reduction stops being worth having:
/// the sweeps that fall under it are the narrow ones, which are cheap at
/// full extent for the same reason they cannot be reduced.
const MIN_REDUCED_HALF_WIDTH: f32 = 2.5;

/// Half-width of the sweep window that softens by `radius_pixels`
/// (already this sheet's pixels) on a plane reduced by `divisor`: the
/// full-extent window the CPU rounds to, divided down. Between texels for
/// every divisor past one, which is why the sweep takes a half-width
/// rather than a tap count.
pub fn box_half_width(radius_pixels: f32, divisor: u32) -> f32 {
    (box_radius_texels(radius_pixels) as f32 + 0.5) / divisor as f32
}

/// The extent divisor a blur of `radius_pixels` (already this sheet's
/// pixels) runs its sweeps at: the widest reduction whose window still
/// spans [`MIN_REDUCED_HALF_WIDTH`], or one for a blur too narrow to
/// reduce — which is every blur on a canvas small enough that its
/// softening is a handful of texels to begin with.
pub fn blur_divisor(radius_pixels: f32) -> u32 {
    REDUCED_DIVISORS
        .into_iter()
        .find(|&divisor| box_half_width(radius_pixels, divisor) >= MIN_REDUCED_HALF_WIDTH)
        .unwrap_or(1)
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
/// [`box_blur_passes`], horizontal then vertical, each a `BoxBlurParams`
/// block (`axis_x`, `axis_y`, `half_width_texels`), then the
/// `ScaleParams` window the reduced chain's two ends read.
pub struct BoxBlurUniforms {
    /// Window half-width in texels of the plane the sweeps run on, from
    /// [`box_half_width`] — so a reduced chain states the window its own
    /// smaller plane carries.
    pub half_width_texels: f32,
    /// The chain's extent divisor, from [`blur_divisor`].
    pub divisor: u32,
}

/// Bytes of one `BoxBlurParams` uniform window.
const BOX_BLUR_WINDOW_BYTES: u32 = 12;

/// Bytes of the `ScaleParams` window, at the end of the chain's blob.
const BOX_SCALE_WINDOW_BYTES: u32 = 4;

impl BoxBlurUniforms {
    /// Total bytes [`Self::encode`] appends at the chain's
    /// `uniform_offset`: the horizontal window, the vertical one, then the
    /// scale window. A full-extent chain lays no pass over the last one
    /// and encodes it all the same, so the layout is the same shape
    /// whatever extent the chain settles on.
    pub const BYTES: u32 = 2 * BOX_BLUR_WINDOW_BYTES + BOX_SCALE_WINDOW_BYTES;

    /// The three windows, packed tight in the blob layout the shader
    /// declares.
    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        encode_words([
            1u32.to_le_bytes(),
            0u32.to_le_bytes(),
            self.half_width_texels.to_le_bytes(),
            0u32.to_le_bytes(),
            1u32.to_le_bytes(),
            self.half_width_texels.to_le_bytes(),
            self.divisor.to_le_bytes(),
        ])
    }
}

/// Where one blur chain sweeps: the two transients its iterations
/// ping-pong between, and the extent they run at. Both transients are
/// [`reduced_plane_slot`]s of the same `divisor` the caller declares
/// (distinct from each other, and from `output` when that is a
/// transient).
pub struct BoxBlurChain {
    pub scratch: u32,
    pub carry: u32,
    /// One for a chain that sweeps the canvas itself, otherwise the
    /// reduction [`blur_divisor`] chose.
    pub divisor: u32,
}

/// The full blur chain mirroring `image::blur`: [`BLUR_PASSES`] box
/// iterations, each a horizontal sweep into the `scratch` transient and a
/// vertical sweep out of it — the vertical result parking in `carry`
/// between iterations. At `divisor` one the last vertical sweep lands
/// straight in `output`; past it the chain opens with a block-average
/// downsample into `carry`, sweeps the reduced plane, and closes with a
/// bilinear upsample out of `carry` into `output`. The chain's
/// [`BoxBlurUniforms`] encode at `uniform_offset`, shared by every pass.
pub fn box_blur_passes(
    source: InputSlot,
    chain: &BoxBlurChain,
    output: OutputSlot,
    uniform_offset: u32,
) -> Vec<ProgramPass> {
    let reduced = chain.divisor > 1;
    let scale_offset = uniform_offset + 2 * BOX_BLUR_WINDOW_BYTES;
    let mut passes = Vec::with_capacity(2 * BLUR_PASSES as usize + 2);
    let mut read = if reduced {
        passes.push(pass(
            BOX_DOWNSAMPLE_ENTRY,
            vec![source],
            OutputSlot::Transient { index: chain.carry },
            scale_offset,
            BOX_SCALE_WINDOW_BYTES,
        ));
        InputSlot::Transient { index: chain.carry }
    } else {
        source
    };

    for iteration in 0..BLUR_PASSES {
        let last = iteration + 1 == BLUR_PASSES;
        let vertical_out = if last && !reduced {
            output
        } else {
            OutputSlot::Transient { index: chain.carry }
        };
        passes.push(pass(
            BOX_BLUR_ENTRY,
            vec![read],
            OutputSlot::Transient { index: chain.scratch },
            uniform_offset,
            BOX_BLUR_WINDOW_BYTES,
        ));
        passes.push(pass(
            BOX_BLUR_ENTRY,
            vec![InputSlot::Transient { index: chain.scratch }],
            vertical_out,
            uniform_offset + BOX_BLUR_WINDOW_BYTES,
            BOX_BLUR_WINDOW_BYTES,
        ));
        read = InputSlot::Transient { index: chain.carry };
    }

    if reduced {
        passes.push(pass(
            BOX_UPSAMPLE_ENTRY,
            vec![InputSlot::Transient { index: chain.carry }],
            output,
            scale_offset,
            BOX_SCALE_WINDOW_BYTES,
        ));
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
            (self.window.0 * RIM_RESTRIDE.0 as u32).to_le_bytes(),
            (self.window.1 * RIM_RESTRIDE.1 as u32).to_le_bytes(),
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
