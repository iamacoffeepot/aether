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
//! Ops read and write data planes: [`plane_slot`]'s `R32Float` where a
//! texel carries a label or an index, and [`soft_plane_slot`]'s filterable
//! `R16Float` where it carries a quantity — which is everywhere a blur
//! chain sweeps, since the pairing that halves its fetches is a filtered
//! read. A chain's final pass may instead target an `Rgba8` writable
//! binding, which quantizes the plane to 8 bits at the very end — the form
//! the parity scenarios observe through the overlay path.

use aether_math::Vec2;
use aether_render::{InputSlot, OutputSlot, PassStage, ProgramPass, SlotExtent, SlotSpec, TextureFormat};

/// The puddle WGSL module: one fragment entry point per op, registered
/// (usually alongside sibling op modules, concatenated) through
/// `aether.render.program.register`.
pub const PUDDLE_WGSL: &str = include_str!("puddle.wgsl");

/// Entry point of one separable box-average sweep ([`box_blur_passes`]).
pub const BOX_BLUR_ENTRY: &str = "fs_box_blur";

/// Entry points of one fused sweep per axis — the chain's whole softening
/// as a single kernel ([`composite_taps`]).
pub const FUSED_BLUR_X_ENTRY: &str = "fs_fused_blur_x";
pub const FUSED_BLUR_Y_ENTRY: &str = "fs_fused_blur_y";
/// Entry points for two same-kernel scalar planes carried in the R/G
/// channels of one filterable target. `PAIR_X` reads the two separate
/// sources; the `PAIRED` entries read the packed intermediate.
pub const FUSED_BLUR_PAIR_X_ENTRY: &str = "fs_fused_blur_pair_x";
pub const FUSED_BLUR_PAIRED_X_ENTRY: &str = "fs_fused_blur_paired_x";
pub const FUSED_BLUR_PAIRED_Y_ENTRY: &str = "fs_fused_blur_paired_y";
pub const BOX_BLUR_PAIR_ENTRY: &str = "fs_box_blur_pair";
pub const BOX_BLUR_PAIRED_ENTRY: &str = "fs_box_blur_paired";

/// Entry point of the carry onto the soft plane ([`soft_carry_pass`]).
pub const SOFT_CARRY_ENTRY: &str = "fs_soft_carry";

/// Entry points of a reduced-extent chain's two ends: the block average
/// into the reduced plane and the bilinear carry back out of it.
pub const BOX_DOWNSAMPLE_ENTRY: &str = "fs_box_downsample";
pub const BOX_UPSAMPLE_ENTRY: &str = "fs_box_upsample";
pub const BOX_DOWNSAMPLE_PAIR_ENTRY: &str = "fs_box_downsample_pair";
pub const BOX_UPSAMPLE_PAIRED_ENTRY: &str = "fs_box_upsample_paired";

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

/// The plane format a blur chain reads and writes, and the one the wash
/// carries its whole develop in: `R16Float`, which core WebGPU filters.
///
/// The blur is what asks for it. A symmetric kernel's taps pair exactly
/// into one filtered read apiece ([`fused_taps`]), which is half the
/// fetches of the same kernel taken point by point — and the sweeps are
/// where a develop's fetches almost all are. Filtering is a property of
/// the format, not of the pass, so the source has to stand at a
/// filterable one for the pairing to be available at all.
///
/// What it costs is mantissa: about eleven bits against the 32-bit
/// plane's twenty-four. A plane carries coverage, density or value, all
/// resolved into an eight-bit sheet at the end, so the quantization sits
/// three bits under the finest step the picture can show. A label or a
/// texel index is the case this is wrong for — the care flood's seeds
/// stay at [`plane_slot`]'s format for exactly that reason.
pub const SOFT_PLANE_FORMAT: TextureFormat = TextureFormat::R16Float;

/// [`SOFT_PLANE_FORMAT`] at the full extent — what a standalone blur
/// scenario declares for the planes it hands the chain.
pub fn soft_plane_slot() -> SlotSpec {
    SlotSpec { format: SOFT_PLANE_FORMAT, extent: SlotExtent::Full }
}

/// The plane a blur chain of [`BoxBlurChain::divisor`] sweeps over: the
/// soft plane at a `divisor`-th of the canvas on each axis. A divisor of
/// one is [`soft_plane_slot`] itself — the executor pools both onto the
/// same texture (ADR-0170), so a full-extent chain declared this way
/// allocates nothing extra.
pub fn reduced_plane_slot(divisor: u32) -> SlotSpec {
    SlotSpec { format: SOFT_PLANE_FORMAT, extent: SlotExtent::Divided { divisor } }
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
pub const MIN_REDUCED_HALF_WIDTH: f32 = 2.5;

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

/// Vectors the fused sweep's uniform block declares, matching the WGSL
/// `array<vec4<f32>, FUSED_BLUR_VECTORS>` this side fills. Each carries
/// two `(offset, weight)` reads, so the block holds twice this many.
const FUSED_WEIGHT_VECTORS: usize = 12;

/// Composite taps one fused kernel carries either side of its centre.
/// Each pair of them rides one filtered read ([`fused_taps`]) and the
/// block holds `2 * FUSED_WEIGHT_VECTORS` reads, so the ceiling is four
/// times the vector count. Three sweeps of reach `r` convolve to a
/// kernel of reach `3r`, so this covers every chain whose sweeps reach
/// fifteen texels — the whole palette on any canvas up to around two and
/// a half thousand pixels tall. A chain past it sweeps its
/// [`BLUR_PASSES`] iterations instead ([`fuses`]).
pub const MAX_FUSED_WEIGHTS: usize = 4 * FUSED_WEIGHT_VECTORS;

/// The chain's [`BLUR_PASSES`] box sweeps convolved into one kernel, from
/// the centre out: `[0]` weights the centre tap and `[i]` the pair at
/// `±i`, normalized so the whole symmetric kernel sums to one.
///
/// Convolution is associative and the two axes commute, so this one
/// kernel per axis is the six sweeps exactly — not a fit to them. The
/// taps it convolves are the sweep's own coverage weights, fractional
/// half-widths included, so a reduced-extent chain fuses on the same
/// terms a full-extent one does.
///
/// Exactly at the plane's border too, which is what its mirrored edge
/// buys (iamacoffeepot/aether#4444): a symmetric extension survives a
/// symmetric kernel, so re-extending the plane between three sweeps lands
/// where extending it once does. Under a replicated edge the two would
/// part within [`BLUR_PASSES`] reaches of every edge, and the wash's
/// threshold would cut the difference into a displaced tide line.
pub fn composite_taps(half_width_texels: f32) -> Vec<f32> {
    let sweep = sweep_taps(half_width_texels);
    let mut kernel = vec![1.0f32];
    for _ in 0..BLUR_PASSES {
        kernel = convolved(&kernel, &sweep);
    }

    let total: f32 = kernel.iter().sum();
    kernel[kernel.len() / 2..].iter().map(|weight| weight / total).collect()
}

/// Whether a sweep of `half_width_texels` fuses, or is wide enough that
/// its composite outruns [`MAX_FUSED_WEIGHTS`] and the chain keeps its
/// iterations.
pub fn fuses(half_width_texels: f32) -> bool {
    BLUR_PASSES as usize * sweep_reach(half_width_texels) < MAX_FUSED_WEIGHTS
}

/// One side of a fused kernel as the reads a filtering sampler answers
/// it in: the centre tap, then one `(offset, weight)` per *pair* of
/// composite taps (iamacoffeepot/aether#4387).
///
/// The pairing is exact rather than an approximation of the kernel.
/// Reading at a fractional offset between texels `i` and `i + 1` hands
/// back `(1 - f) * t[i] + f * t[i + 1]`, so a read at
/// `(i * w[i] + (i + 1) * w[i + 1]) / (w[i] + w[i + 1])` scaled by
/// `w[i] + w[i + 1]` is `w[i] * t[i] + w[i + 1] * t[i + 1]` — the two
/// taps the kernel wanted, out of one fetch instead of two. The sweeps
/// are where nearly every fetch in a develop is, so halving them is the
/// develop's own pacing; what pays for it is the plane standing at a
/// filterable format ([`SOFT_PLANE_FORMAT`]) and the sampler's own
/// sub-texel weight, which is finer than the eight-bit sheet the plane
/// resolves into.
///
/// A trailing unpaired tap rides alone with its own integer offset, so
/// an even and an odd kernel both come out exact.
pub fn fused_taps(half_width_texels: f32) -> (f32, Vec<(f32, f32)>) {
    let kernel = composite_taps(half_width_texels);
    let mut taps = Vec::with_capacity(kernel.len() / 2);
    for (near, pair) in kernel[1..].chunks(2).enumerate() {
        let (first, second) = (pair[0], pair.get(1).copied().unwrap_or(0.0));
        let weight = first + second;
        if weight <= 0.0 {
            continue;
        }
        let near = (1 + 2 * near) as f32;
        taps.push(((near * first + (near + 1.0) * second) / weight, weight));
    }

    (kernel[0], taps)
}

/// One sweep's taps: how much of its own texel the window covers, which
/// is what the sweep loop pays — one for every tap inside, the straddle
/// for the pair at each end.
fn sweep_taps(half_width_texels: f32) -> Vec<f32> {
    let reach = sweep_reach(half_width_texels);
    if reach == 0 {
        return vec![2.0 * half_width_texels];
    }

    let mut taps = vec![1.0; 2 * reach + 1];
    let straddle = half_width_texels - reach as f32 + 0.5;
    taps[0] = straddle;
    taps[2 * reach] = straddle;
    taps
}

/// Taps one sweep of `half_width_texels` reaches either side of centre —
/// the shader's own `ceil(half_width - 0.5)`.
fn sweep_reach(half_width_texels: f32) -> usize {
    (half_width_texels - 0.5).ceil().max(0.0) as usize
}

fn convolved(left: &[f32], right: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0; left.len() + right.len() - 1];
    for (at, value) in left.iter().enumerate() {
        for (offset, weight) in right.iter().enumerate() {
            out[at + offset] += value * weight;
        }
    }
    out
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

/// Uniforms for one blur chain: the `FusedBlurParams` block both fused
/// sweeps share, then the two `BoxBlurParams` windows an unfused chain's
/// iterations read (`axis_x`, `axis_y`, `half_width_texels`), then the
/// `ScaleParams` window the reduced chain's two ends read.
pub struct BoxBlurUniforms {
    /// Window half-width in texels of the plane the sweeps run on, from
    /// [`box_half_width`] — so a reduced chain states the window its own
    /// smaller plane carries.
    pub half_width_texels: f32,
    /// The chain's extent divisor, from [`blur_divisor`].
    pub divisor: u32,
}

/// Bytes of the `FusedBlurParams` window: the read count and the centre
/// weight, the padding the uniform address space's sixteen-byte array
/// stride forces, then the packed `(offset, weight)` reads.
const FUSED_BLUR_WINDOW_BYTES: u32 = 16 + 16 * FUSED_WEIGHT_VECTORS as u32;

/// Bytes of one `BoxBlurParams` uniform window.
const BOX_BLUR_WINDOW_BYTES: u32 = 12;

/// Bytes of the `ScaleParams` window, at the end of the chain's blob.
const BOX_SCALE_WINDOW_BYTES: u32 = 4;

impl BoxBlurUniforms {
    /// Total bytes [`Self::encode`] appends at the chain's
    /// `uniform_offset`: the fused window, the two sweep windows, then the
    /// scale window. A chain lays passes over some of these and encodes
    /// them all the same, so the layout is the same shape whatever extent
    /// and whatever kernel the chain settles on.
    pub const BYTES: u32 = FUSED_BLUR_WINDOW_BYTES + 2 * BOX_BLUR_WINDOW_BYTES + BOX_SCALE_WINDOW_BYTES;

    /// The four windows, in the blob layout the shader declares.
    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        if fuses(self.half_width_texels) {
            let (centre, taps) = fused_taps(self.half_width_texels);
            bytes[0..4].copy_from_slice(&(taps.len() as u32).to_le_bytes());
            bytes[4..8].copy_from_slice(&centre.to_le_bytes());
            for (slot, (offset, weight)) in bytes[16..].chunks_exact_mut(8).zip(&taps) {
                slot[0..4].copy_from_slice(&offset.to_le_bytes());
                slot[4..8].copy_from_slice(&weight.to_le_bytes());
            }
        }

        bytes[FUSED_BLUR_WINDOW_BYTES as usize..].copy_from_slice(&encode_words::<7, 28>([
            1u32.to_le_bytes(),
            0u32.to_le_bytes(),
            self.half_width_texels.to_le_bytes(),
            0u32.to_le_bytes(),
            1u32.to_le_bytes(),
            self.half_width_texels.to_le_bytes(),
            self.divisor.to_le_bytes(),
        ]));
        bytes
    }
}

/// Where one blur chain sweeps: the transients it hops through, the
/// extent they run at, and the window they carry. Both transients are
/// [`reduced_plane_slot`]s of the same `divisor` the caller declares
/// (distinct from each other, and from `output` when that is a
/// transient); a full-extent fused chain hops through `scratch` alone and
/// leaves `carry` unwritten.
pub struct BoxBlurChain {
    pub scratch: u32,
    pub carry: u32,
    /// One for a chain that sweeps the canvas itself, otherwise the
    /// reduction [`blur_divisor`] chose.
    pub divisor: u32,
    /// The sweep window on that plane, from [`box_half_width`] — what
    /// decides whether the chain fuses ([`fuses`]).
    pub half_width_texels: f32,
}

/// The full blur chain mirroring `image::blur`: one fused sweep per axis,
/// horizontal into the `scratch` transient and vertical out of it, both
/// reading the one composite kernel [`composite_taps`] convolved from the
/// chain's [`BLUR_PASSES`] iterations. A chain too wide to fuse
/// ([`fuses`]) sweeps those iterations instead, the vertical result
/// parking in `carry` between them.
///
/// At `divisor` one the vertical sweep lands straight in `output`; past
/// it the chain opens with a block-average downsample into `carry`,
/// sweeps the reduced plane, and closes with a bilinear upsample out of
/// `carry` into `output`. The chain's [`BoxBlurUniforms`] encode at
/// `uniform_offset`, shared by every pass.
pub fn box_blur_passes(
    source: InputSlot,
    chain: &BoxBlurChain,
    output: OutputSlot,
    uniform_offset: u32,
) -> Vec<ProgramPass> {
    let reduced = chain.divisor > 1;
    let sweep_offset = uniform_offset + FUSED_BLUR_WINDOW_BYTES;
    let scale_offset = sweep_offset + 2 * BOX_BLUR_WINDOW_BYTES;
    let mut passes = Vec::with_capacity(2 * BLUR_PASSES as usize + 2);

    let source = if reduced {
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
    let softened = if reduced {
        OutputSlot::Transient { index: chain.carry }
    } else {
        output
    };

    if fuses(chain.half_width_texels) {
        passes.push(pass(
            FUSED_BLUR_X_ENTRY,
            vec![source],
            OutputSlot::Transient { index: chain.scratch },
            uniform_offset,
            FUSED_BLUR_WINDOW_BYTES,
        ));
        passes.push(pass(
            FUSED_BLUR_Y_ENTRY,
            vec![InputSlot::Transient { index: chain.scratch }],
            softened,
            uniform_offset,
            FUSED_BLUR_WINDOW_BYTES,
        ));
    } else {
        let mut read = source;
        for iteration in 0..BLUR_PASSES {
            let last = iteration + 1 == BLUR_PASSES;
            passes.push(pass(
                BOX_BLUR_ENTRY,
                vec![read],
                OutputSlot::Transient { index: chain.scratch },
                sweep_offset,
                BOX_BLUR_WINDOW_BYTES,
            ));
            passes.push(pass(
                BOX_BLUR_ENTRY,
                vec![InputSlot::Transient { index: chain.scratch }],
                if last {
                    softened
                } else {
                    OutputSlot::Transient { index: chain.carry }
                },
                sweep_offset + BOX_BLUR_WINDOW_BYTES,
                BOX_BLUR_WINDOW_BYTES,
            ));
            read = InputSlot::Transient { index: chain.carry };
        }
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

/// Two scalar blur chains with one radius and extent, carried through R/G
/// of one `Rgba16Float` plane. The first pass reads `sources` separately;
/// every later pass reads the paired intermediate. Each lane executes the
/// scalar chain's arithmetic and lands in an `f16` channel, so pairing
/// removes one chain without changing either plane's precision.
pub fn paired_box_blur_passes(
    sources: [InputSlot; 2],
    chain: &BoxBlurChain,
    output: OutputSlot,
    uniform_offset: u32,
) -> Vec<ProgramPass> {
    let reduced = chain.divisor > 1;
    let sweep_offset = uniform_offset + FUSED_BLUR_WINDOW_BYTES;
    let scale_offset = sweep_offset + 2 * BOX_BLUR_WINDOW_BYTES;
    let mut passes = Vec::with_capacity(2 * BLUR_PASSES as usize + 2);

    if reduced {
        passes.push(pass(
            BOX_DOWNSAMPLE_PAIR_ENTRY,
            sources.to_vec(),
            OutputSlot::Transient { index: chain.carry },
            scale_offset,
            BOX_SCALE_WINDOW_BYTES,
        ));
    }
    let softened = if reduced {
        OutputSlot::Transient { index: chain.carry }
    } else {
        output
    };

    if fuses(chain.half_width_texels) {
        passes.push(pass(
            if reduced {
                FUSED_BLUR_PAIRED_X_ENTRY
            } else {
                FUSED_BLUR_PAIR_X_ENTRY
            },
            if reduced {
                vec![InputSlot::Transient { index: chain.carry }]
            } else {
                sources.to_vec()
            },
            OutputSlot::Transient { index: chain.scratch },
            uniform_offset,
            FUSED_BLUR_WINDOW_BYTES,
        ));
        passes.push(pass(
            FUSED_BLUR_PAIRED_Y_ENTRY,
            vec![InputSlot::Transient { index: chain.scratch }],
            softened,
            uniform_offset,
            FUSED_BLUR_WINDOW_BYTES,
        ));
    } else {
        for iteration in 0..BLUR_PASSES {
            let first = iteration == 0 && !reduced;
            passes.push(pass(
                if first {
                    BOX_BLUR_PAIR_ENTRY
                } else {
                    BOX_BLUR_PAIRED_ENTRY
                },
                if first {
                    sources.to_vec()
                } else {
                    vec![InputSlot::Transient { index: chain.carry }]
                },
                OutputSlot::Transient { index: chain.scratch },
                sweep_offset,
                BOX_BLUR_WINDOW_BYTES,
            ));
            let last = iteration + 1 == BLUR_PASSES;
            passes.push(pass(
                BOX_BLUR_PAIRED_ENTRY,
                vec![InputSlot::Transient { index: chain.scratch }],
                if last {
                    softened
                } else {
                    OutputSlot::Transient { index: chain.carry }
                },
                sweep_offset + BOX_BLUR_WINDOW_BYTES,
                BOX_BLUR_WINDOW_BYTES,
            ));
        }
    }

    if reduced {
        passes.push(pass(
            BOX_UPSAMPLE_PAIRED_ENTRY,
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

/// The carry onto the soft plane: one pointwise pass taking a plane a
/// chain was handed from outside the graph — a binding another program
/// wrote, a texture staged from the CPU — onto [`SOFT_PLANE_FORMAT`], so
/// the sweeps that read it can pair their taps through a filtering
/// sampler. Takes no uniform window.
pub fn soft_carry_pass(source: InputSlot, output: OutputSlot) -> ProgramPass {
    pass(SOFT_CARRY_ENTRY, vec![source], output, 0, 0)
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

#[cfg(test)]
mod tests {
    use super::{BLUR_PASSES, MAX_FUSED_WEIGHTS, composite_taps, fuses, sweep_reach};
    use crate::easel::image;
    use crate::math3::hash_unit;

    /// A plane one texel tall, so `image::blur`'s vertical sweep reads the
    /// one row it has and the call is exactly [`BLUR_PASSES`] horizontal
    /// box passes over `field` — the iterated chain this kernel replaces.
    fn iterated(field: &[f32], radius_pixels: f32) -> Vec<f32> {
        image::blur(field, field.len(), 1, radius_pixels)
    }

    /// The composite kernel swept once, the edge mirrored as the sweeps
    /// mirror theirs (`image::reflected`, transcribed).
    fn fused(field: &[f32], half_width_texels: f32) -> Vec<f32> {
        let taps = composite_taps(half_width_texels);
        let last = field.len() as isize - 1;
        (0..field.len())
            .map(|at| {
                let read = |offset: isize| {
                    let index = at as isize + offset;
                    let under = if index < 0 {
                        -1 - index
                    } else {
                        index
                    };
                    let over = if under > last {
                        2 * last + 1 - under
                    } else {
                        under
                    };
                    field[over.clamp(0, last) as usize]
                };
                taps.iter()
                    .zip(0isize..)
                    .map(|(weight, tap)| match tap {
                        0 => weight * read(0),
                        _ => weight * (read(-tap) + read(tap)),
                    })
                    .sum()
            })
            .collect()
    }

    /// The whole claim the fusion rests on: one composite sweep is the
    /// three box sweeps, not a fit to them — over the whole field, border
    /// rows included, because the plane's edge is a mirror
    /// (iamacoffeepot/aether#4444) and a symmetric extension survives a
    /// symmetric kernel. Under a replicated edge this would hold only
    /// away from the border, and the border is where the wash's threshold
    /// turns a fraction of a step into a displaced tide line.
    ///
    /// The named bugs, each of which moves whole percentages of the
    /// signal: a kernel convolved twice or four times rather than
    /// [`BLUR_PASSES`], a straddling tap taken as a whole one (the window
    /// widens by up to a texel per sweep), a kernel normalized against
    /// its one-sided sum rather than the whole symmetric one (everything
    /// darkens by nearly half), and a reach short by one (the outermost
    /// pair silently dropped).
    ///
    /// Tripwire: a composite sweep is the iterated chain, to a hair over
    /// float rounding.
    #[test]
    fn one_composite_sweep_is_the_three_box_sweeps() {
        let field: Vec<f32> = (0..96u64).map(hash_unit).collect();

        for radius_pixels in [1.7, 3.4, 5.1, 8.5, 13.6] {
            let half_width = super::box_radius_texels(radius_pixels) as f32 + 0.5;
            let (want, got) = (iterated(&field, radius_pixels), fused(&field, half_width));

            let worst = want.iter().zip(&got).map(|(want, got)| (want - got).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-5, "radius {radius_pixels}: the composite sweep drifts {worst} from three box sweeps");
        }
    }

    /// A kernel wide enough to outrun the uniform's weight table keeps its
    /// iterations, and every kernel the fused sweep accepts fits in it —
    /// so the pass builder's two shapes and the block it fills agree on
    /// where the line is.
    ///
    /// Tripwire: the widest accepted kernel exactly fills the table.
    #[test]
    fn the_fused_kernel_fits_the_weights_the_block_declares() {
        for half_width in [0.5f32, 1.5, 2.5, 3.375, 7.375, 15.5, 16.5, 40.0] {
            let taps = composite_taps(half_width);
            assert_eq!(
                fuses(half_width),
                taps.len() <= MAX_FUSED_WEIGHTS,
                "half-width {half_width} ({} weights) disagrees with the block's room",
                taps.len(),
            );
            assert_eq!(taps.len(), BLUR_PASSES as usize * sweep_reach(half_width) + 1, "reach {half_width}");
        }
    }

    /// Every read the fused sweep makes, unpacked back into the two taps
    /// a filtering sampler resolves it into, must be the composite kernel
    /// it was folded from. A read at offset `o` between texels `i` and
    /// `i + 1` is answered `(1 - f) * t[i] + f * t[i + 1]` for
    /// `f = o - i`, so scaling that by the read's weight has to give back
    /// `w[i] * t[i] + w[i + 1] * t[i + 1]` — which is the whole reason
    /// the pairing is exact rather than an approximation of the kernel.
    ///
    /// The named bugs, each of which softens by the wrong amount without
    /// looking obviously wrong: an offset averaged rather than weighted
    /// (the pair's split ignores which tap is heavier), the two weights
    /// transposed in that average (the read leans the wrong way), a
    /// weight halved because the pair was read as one tap counted twice,
    /// and a trailing odd tap folded against a zero as though it had a
    /// partner (its offset drifts a half-texel outward).
    ///
    /// Tripwire: the reads unpack to the kernel, taps and weights both.
    #[test]
    fn the_reads_a_fused_sweep_makes_unpack_to_the_kernel() {
        for half_width in [0.5f32, 1.5, 2.5, 3.375, 7.375, 15.5] {
            let kernel = composite_taps(half_width);
            let (centre, reads) = super::fused_taps(half_width);
            assert!((centre - kernel[0]).abs() < 1e-6, "half-width {half_width}: the centre tap moved");

            let mut unpacked = vec![0.0f32; kernel.len()];
            unpacked[0] = centre;
            for &(offset, weight) in &reads {
                let near = offset.floor();
                let fraction = offset - near;
                let near = near as usize;
                unpacked[near] += weight * (1.0 - fraction);
                if near + 1 < unpacked.len() {
                    unpacked[near + 1] += weight * fraction;
                }
            }

            let worst = kernel.iter().zip(&unpacked).map(|(want, got)| (want - got).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-6, "half-width {half_width}: an unpacked read drifts {worst} from the kernel");
        }
    }
}
