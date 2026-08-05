//! The coat sequencer (iamacoffeepot/aether#4369, #4387): the whole of
//! `field::Sheet::coats` plus `palette::composite` composed from the op
//! builders into one registered ADR-0170 program — and, since #4387, the
//! whole develop, with nothing left on the CPU that scales with the
//! canvas.
//!
//! [`program`] lays the graph once, statically. First the three fields
//! the develop used to be handed as uploaded planes — the flow off the
//! ink coverage plane the ink layer derives
//! ([`super::flow`]), how closely the hand is held off the bake's class
//! channel ([`super::care`]), and the face paint off the chart's aperture
//! loops ([`super::face`]) — and then, for every entry in
//! [`palette::MATERIALS`], a coverage mask, a value plane, a tight wash, a
//! loose wash under the care ramp for a material the hand relaxes over,
//! the flow smear for the hair, the lid lift for the iris, the glaze
//! dropped into the wet hair, the atmosphere stain for a material that
//! throws one — each wash's density absorbed into the light accumulator in
//! the same coat order the CPU composites — and the final resolve against
//! paper white into the `Rgba8` sheet binding.
//!
//! # What a dispatch supplies
//!
//! Six sampled textures and one writable one ([`WashBindings`] plus the
//! ink coverage plane), where
//! there were thirteen. The class, tone and facing planes collapsed into
//! the one packed plane the bake pass writes (#4420); care, flow, lift,
//! iris and blush became passes here; and the paper's three noise fields
//! are a pure function of the seed and the canvas, so they are pulped once
//! when a canvas is created ([`field::paper`]) rather than re-derived and
//! re-uploaded per develop. Nothing is staged per frame but the uniform
//! blob and the one geometry the chart moves.
//!
//! # Two extents
//!
//! The wash body develops at [`BODY_DIVISOR`] times coarser than the sheet
//! it lands on — the `SlotExtent::Divided` notch, on ADR-0170's
//! frequency-split argument — and the accents do not. A wash is water and
//! pigment finding its own edge over tens of pixels; an iris is a couple
//! of dozen pixels across at the framing the engine is tuned for and its
//! slit a fraction of one. So the coarse half is the whole labelled box
//! and the cheek flush, and the fine half is the aperture clip, the iris'
//! own wash, the lid weight over it and the paper grain the sheet resolves
//! against. `Grain` is which of the two a chain develops at; the seam
//! between them is one bilinear lift of the light accumulator
//! (`fs_light_lift`), taken after the last coarse coat and before the
//! first fine one, which multiplicative absorption lets us reorder freely.
//!
//! # Chance and placement
//!
//! Chance stays on the CPU: the dispatch encoder replays the exact
//! accident stream the oracle draws — one [`field::WashAccidents::roll`]
//! per wash whose mask has coverage, in `coats`' own material order, and
//! the atmosphere's own salted stream — so the blob carries the same
//! jitters, noise windows and drops the CPU develop consumes. None of it
//! turns with the view, so it is rolled into a [`SeedUniforms`] slice once
//! and re-placed per frame ([`WashProgram::frame_uniforms`]): what a frame
//! writes is the two face frames and one centroid per
//! chain. Where those centroids come from is [`super::super::survey`] —
//! the class plane lives on the GPU now and ADR-0170 declines a readback,
//! so they are measured off the subject's own geometry instead.
//!
//! The CPU wash remains the oracle; `tests/program_wash_scenario.rs` holds
//! the two whole-sheet develops together.

use aether_math::Vec2;
use aether_render::{
    InputSlot, OutputSlot, PassStage, ProgramPass, ProgramRegister, SlotExtent, SlotSpec, TextureFormat,
};

use crate::easel::accent::{self, Eye};
use crate::easel::field::{self, WashParams};
use crate::easel::image::{self, Rng};
use crate::easel::palette;
use crate::easel::survey;
use crate::labels::{HAIR, SKIN};

use super::sheet::{
    CoatParams, LostEdgeParams, SHEET_PARAMS_BYTES, care_mix_pass, coat_absorb_pass, light_prime_pass, lost_edge_pass,
    paper_composite_pass, sheet_slot,
};
use super::{care, face, flow, pigment, puddle};

/// The sequencer's own WGSL: the glue entry points between the op
/// modules. Never registered alone — [`module`] concatenates it after
/// the op modules whose helpers it calls.
pub const WASH_WGSL: &str = include_str!("wash.wgsl");

/// The wash program's one WGSL module: every op module plus the
/// sequencer's glue, concatenated in dependency order — [`puddle`] first,
/// since its `hermite` and `plane_out` are what everything below calls.
pub fn module() -> String {
    [
        puddle::PUDDLE_WGSL,
        pigment::PIGMENT_WGSL,
        super::sheet::SHEET_WGSL,
        care::CARE_WGSL,
        flow::FLOW_WGSL,
        face::FACE_WGSL,
        WASH_WGSL,
    ]
    .join("\n")
}

/// How much coarser the wash body develops than the sheet it lands on.
///
/// Two, and the argument is ADR-0170's own: the body carries the low
/// frequencies, the accents carry the high ones, and only the second needs
/// the sheet's own pixels. One would turn the notch off — `SlotExtent::Full`
/// and `Divided { divisor: 1 }` resolve to the same texture, and the seam
/// pass degenerates to an exact copy — which is what the parity scenario
/// leans on to separate a notch regression from a wiring one.
pub const BODY_DIVISOR: u32 = 2;

/// Where the accents develop. The body's extent is the graph's own
/// ([`Graph::body`]), since [`program_at`] lays it at whatever divisor it
/// is asked for.
const FINE: SlotExtent = SlotExtent::Full;

/// Dispatch-binding indices, in the order [`program`] declares them and
/// [`WashBindings::dispatched`] lists them.
const PACKED: u32 = 0;
const TOOTH: u32 = 1;
const EDGE: u32 = 2;
const TOOTH_FINE: u32 = 3;
const EDGE_FINE: u32 = 4;
const PAPER_SHADE: u32 = 5;
const SHEET: u32 = 6;
const INK: u32 = 7;

/// The geometry slot the program declares, filled by id per dispatch
/// (ADR-0171): the chart's aperture loops the face pass fills.
const APERTURE_GEOMETRY: u32 = 0;

/// The registry textures the easel itself creates for one canvas, named.
///
/// Seven, and only the last is written. Every one is created once with
/// its canvas and outlives any number of develops: the packed plane is
/// the bake program's own output (a `Writable` texture this program
/// samples), and the four data planes are pulped from the seed when the
/// canvas is. The eighth binding a dispatch supplies — the ink coverage
/// plane — is the ink layer's, and joins these in
/// [`WashBindings::dispatched`].
pub struct WashBindings {
    /// The bake's packed plane at the body's extent: the region class in
    /// R, how the key light falls in G, how far the surface turns toward
    /// the eye in B ([`super::bake`]).
    pub packed: u32,
    /// The paper's tooth and its tide-line field
    /// ([`field::NoisePlanes`]) at the body's extent, where every wash
    /// granulates and every edge is dithered.
    pub tooth: u32,
    pub edge: u32,
    /// The same two fields at the sheet's own extent, for the one chain
    /// that develops there — the iris.
    pub tooth_fine: u32,
    pub edge_fine: u32,
    /// The sheet's own colour variation ([`field::Paper::shade`]), which
    /// the composite resolves against and so is never notched.
    pub paper_shade: u32,
    /// The developed sheet, written by the final composite pass.
    pub sheet: u32,
}

impl WashBindings {
    /// The seven textures the easel creates and releases with its own
    /// canvas. The ink coverage plane is not among them — it belongs to
    /// the ink layer, which writes it (see [`dispatched`]).
    ///
    /// [`dispatched`]: WashBindings::dispatched
    pub fn owned(&self) -> Vec<u32> {
        vec![self.packed, self.tooth, self.edge, self.tooth_fine, self.edge_fine, self.paper_shade, self.sheet]
    }

    /// The `ProgramDispatch::bindings` list, in declaration order: the
    /// easel's own seven, then the ink coverage plane the ink layer's
    /// [`stroke`](super::stroke) program derived this frame.
    pub fn dispatched(&self, ink: u32) -> Vec<u32> {
        let mut bindings = self.owned();
        bindings.push(ink);

        bindings
    }
}

/// The two extents one develop paints at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canvas {
    /// The sheet's own pixels.
    pub width: usize,
    pub height: usize,
}

impl Canvas {
    /// The notched body's pixels — floor division clamped to one, exactly
    /// as the executor resolves [`SlotExtent::Divided`], so what the
    /// encoder tunes its radii at is what the passes run at.
    #[must_use]
    pub fn body(self) -> (usize, usize) {
        self.body_at(BODY_DIVISOR)
    }

    /// The same, at a divisor the caller names — what a graph laid by
    /// [`program_at`] tunes its radii against.
    #[must_use]
    pub fn body_at(self, divisor: u32) -> (usize, usize) {
        let divisor = divisor as usize;

        ((self.width / divisor).max(1), (self.height / divisor).max(1))
    }
}

/// Which of the two extents a chain develops at, and what it reads there.
///
/// The paper's grain is bound twice, once per extent, rather than sampled
/// across the seam: tooth and tide-line noise are the highest-frequency
/// fields in the whole develop, and a chain that read the other extent's
/// copy would granulate against a grain twice the size of its own texels.
#[derive(Clone, Copy)]
struct Grain {
    extent: SlotExtent,
    tooth: InputSlot,
    edge: InputSlot,
    /// Texels of the packed bake plane per texel at this extent — what
    /// `fs_shade` scales its read by.
    source_scale: f32,
    /// Which side of the seam this grain sits on.
    fine: bool,
}

impl Grain {
    /// The coarse side: everything but the accents.
    const fn body(extent: SlotExtent) -> Self {
        Self { extent, tooth: binding(TOOTH), edge: binding(EDGE), source_scale: 1.0, fine: false }
    }

    /// The sheet's own pixels, and the packed plane read at the divisor's
    /// reciprocal because the bake itself develops on the coarse side.
    const fn accent(divisor: u32) -> Self {
        Self {
            extent: FINE,
            tooth: binding(TOOTH_FINE),
            edge: binding(EDGE_FINE),
            source_scale: 1.0 / divisor as f32,
            fine: true,
        }
    }

    /// The pixels a chain at this grain develops at.
    fn pixels(self, canvas: Canvas, divisor: u32) -> (usize, usize) {
        if self.fine {
            (canvas.width, canvas.height)
        } else {
            canvas.body_at(divisor)
        }
    }
}

/// Uniform window for `fs_mask` — the WGSL `MaskParams` block.
pub struct MaskUniforms {
    /// The class this mask selects.
    pub material_class: u8,
    /// Select every labelled texel instead (the figure mask).
    pub figure: bool,
}

impl MaskUniforms {
    pub const BYTES: u32 = 8;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        bytes[0..4].copy_from_slice(&f32::from(self.material_class).to_le_bytes());
        bytes[4..8].copy_from_slice(&u32::from(self.figure).to_le_bytes());
        bytes
    }
}

/// Uniform window for `fs_shade` — the WGSL `ShadeParams` block.
pub struct ShadeUniforms {
    /// How much of the material survives full light.
    pub shade_floor: f32,
    /// Tone at which the material counts as fully lit, already resolved
    /// against `palette`'s default.
    pub lit: f32,
    /// Texels of the bound tone plane per texel of this pass's output.
    pub source_scale: f32,
}

impl ShadeUniforms {
    pub const BYTES: u32 = 12;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        bytes[0..4].copy_from_slice(&self.shade_floor.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.lit.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.source_scale.to_le_bytes());
        bytes
    }
}

/// Uniform window for `fs_pour_accumulate` — the WGSL `AccumulateParams`
/// block.
pub struct AccumulateUniforms {
    /// One to keep the density already poured, zero on a chain's first
    /// pour.
    pub keep: f32,
    /// The pour's body times the wash's load; zero neutralizes the pour.
    pub body_load: f32,
    /// One when the wash carries a value plane.
    pub has_value: f32,
}

impl AccumulateUniforms {
    pub const BYTES: u32 = 12;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        bytes[0..4].copy_from_slice(&self.keep.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.body_load.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.has_value.to_le_bytes());
        bytes
    }
}

/// Uniform window for `fs_lift` — the WGSL `LiftParams` block.
pub struct LiftUniforms {
    /// One applies the lift plane, zero leaves the density untouched.
    pub gate: f32,
}

impl LiftUniforms {
    pub const BYTES: u32 = 4;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        self.gate.to_le_bytes()
    }
}

/// Uniform window for `fs_light_lift` — the WGSL `LiftExtentParams`
/// block, the notch's one seam.
pub struct LiftExtentUniforms {
    pub source_scale: f32,
}

impl LiftExtentUniforms {
    pub const BYTES: u32 = 4;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        self.source_scale.to_le_bytes()
    }
}

/// Uniform window for `fs_atmosphere_spill` — the WGSL `SpillParams`
/// block.
pub struct SpillUniforms {
    /// The material's atmosphere drift, already tuned to this sheet's
    /// pixels.
    pub drift: Vec2,
}

impl SpillUniforms {
    pub const BYTES: u32 = 8;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        bytes[0..4].copy_from_slice(&self.drift.x.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.drift.y.to_le_bytes());
        bytes
    }
}

/// One blur chain's slice of the plan. The radius and the extent are
/// resolved when the graph is laid rather than when the blob is written:
/// both follow from the wash's params and the canvas alone, and the
/// extent has to be known at register time regardless, so resolving them
/// together is what keeps the two in step.
struct BlurPlan {
    window: u32,
    half_width_texels: f32,
    divisor: u32,
}

impl BlurPlan {
    fn encode(&self) -> [u8; puddle::BoxBlurUniforms::BYTES as usize] {
        puddle::BoxBlurUniforms { half_width_texels: self.half_width_texels, divisor: self.divisor }.encode()
    }
}

/// One pour's uniform windows, in pass order along its chain.
struct PourPlan {
    /// `None` when the chain never resamples (a tight wash's single
    /// full-size pour, which the CPU also skips).
    shrink: Option<u32>,
    soft_blur: BlurPlan,
    /// `None` for a wash on a level sheet.
    sag: Option<u32>,
    threshold: u32,
    /// `None` for a wash that holds its whole edge.
    lost: Option<u32>,
    interior_blur: BlurPlan,
    rim: u32,
    accumulate: u32,
}

/// One wash's uniform windows: the support blurs, the pours, and the
/// pigment settle.
struct ChainPlan {
    /// `None` for a wash that carries pigment uniformly (no value plane,
    /// so no support blurs at all).
    support_value_blur: Option<BlurPlan>,
    support_reference_blur: Option<BlurPlan>,
    pours: Vec<PourPlan>,
    granulate: u32,
    /// `None` for a wash that never spatters.
    spatter: Option<u32>,
}

impl ChainPlan {
    /// Every window in this chain whose first eight bytes are the
    /// region's centroid — the three ops that place paint about it.
    ///
    /// This is the whole of what a frame rewrites in a chain: the shrink's
    /// jitter and scale, the lost edge's bearing and the spatter's drops
    /// are all rolled from the seed and sit past those eight bytes.
    fn centre_windows(&self) -> impl Iterator<Item = u32> + '_ {
        self.pours.iter().flat_map(|pour| pour.shrink.into_iter().chain(pour.lost)).chain(self.spatter)
    }
}

/// The atmosphere stain's windows: the figure mask, the two halo blurs,
/// the displaced spill cut, then a wash like any other.
struct AtmospherePlan {
    figure: u32,
    halo_blur: BlurPlan,
    standing_blur: BlurPlan,
    spill: u32,
    chain: ChainPlan,
    coat: u32,
}

/// One material's slice of the program: every window its passes read, in
/// the order [`program`] laid them.
struct MaterialPlan {
    /// `None` for a meta-material, whose mask arrives from the face
    /// passes rather than from the class channel.
    mask: Option<u32>,
    shade: u32,
    tight: ChainPlan,
    /// `None` for a feature too small to loosen.
    loose: Option<ChainPlan>,
    /// The hair's flow smear window.
    smear: Option<u32>,
    /// The iris' lid lift window.
    lift: Option<u32>,
    coat: u32,
    /// The hair's glaze: its wash and its coat window.
    glaze: Option<(ChainPlan, u32)>,
    atmosphere: Option<AtmospherePlan>,
    /// Which extent this material develops at, which decides both where
    /// its radii are tuned and which grain it granulates against. The
    /// iris alone is fine; everything else is body.
    grain: Grain,
}

/// The flow solve's windows: the gradient softening, then per tensor
/// component a selector and a pooling, then the three resolves.
struct FlowPlan {
    gradient_blur: BlurPlan,
    tensors: Vec<(u32, BlurPlan)>,
    resolves: Vec<u32>,
}

/// The face paint's windows. Two eye frames rather than one: the iris and
/// its lid develop at the sheet's own pixels and the cheek flush at the
/// body's, and every quantity in [`face::EyeUniform`] but the pupil
/// fractions and the presence is in canvas pixels.
struct FacePlan {
    fine_eyes: u32,
    body_eyes: u32,
    clip_blur: BlurPlan,
    skin_mask: u32,
    skin_blur: BlurPlan,
    flush_blur: BlurPlan,
}

/// The wash as one registered program: the static graph plus the window
/// layout its dispatch encoder writes into.
pub struct WashProgram {
    /// How much coarser the body developed than the sheet — [`program`]'s
    /// [`BODY_DIVISOR`] in production, whatever [`program_at`] was asked
    /// for otherwise.
    divisor: u32,
    register: ProgramRegister,
    materials: Vec<MaterialPlan>,
    flow: FlowPlan,
    face: FacePlan,
    care: u32,
    blush_coat: u32,
    seam: u32,
    uniform_bytes: u32,
    canvas_height: usize,
}

/// The graph under construction: passes in sequence, one fresh transient
/// per intermediate (the executor pools them by live range), and a
/// bump-allocated uniform layout. `canvas_height` is what every authored
/// radius resolves through — at the extent the plane it softens stands at,
/// so a body chain measures its radius against the body's own pixels — and
/// so it is the one thing the structure, not only the blob, depends on.
struct Graph {
    /// Where the coarse side develops — `Divided { divisor }`, which the
    /// executor resolves to the same texture as `Full` when the divisor
    /// is one.
    body: SlotExtent,
    divisor: u32,
    passes: Vec<ProgramPass>,
    transients: Vec<SlotSpec>,
    uniform_bytes: u32,
    canvas_height: usize,
}

const fn binding(index: u32) -> InputSlot {
    InputSlot::Binding { index }
}

const fn transient(index: u32) -> InputSlot {
    InputSlot::Transient { index }
}

/// Where a quantity the develop paints with stands: the filterable soft
/// plane, so the sweeps that read it can pair their taps
/// ([`puddle::SOFT_PLANE_FORMAT`]). Every intermediate in the graph is
/// one of these but the care flood's two hops, which carry texel indices
/// rather than quantities and stand at [`plane_slot`] instead.
const fn soft_slot(extent: SlotExtent) -> SlotSpec {
    SlotSpec { format: puddle::SOFT_PLANE_FORMAT, extent }
}

/// The 32-bit data plane: what a binding staged from the CPU or written
/// by another program stands at, and what the care flood's seeds need. A
/// seed is a linear texel index, an integer past a million on this
/// canvas, which would not survive eleven bits of mantissa.
const fn plane_slot(extent: SlotExtent) -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent }
}

const fn light_slot(extent: SlotExtent) -> SlotSpec {
    SlotSpec { format: TextureFormat::Rgba8, extent }
}

/// `extent` reduced a further `divisor` on each axis. Both reductions
/// are floor divisions of the reference extent, and a floor division by
/// one integer then another is the floor division by their product, so
/// the compounded extent is exactly the reduction of the plane it is
/// taken against — a blur chain over the body reduces from the body's
/// texels, not from the sheet's.
const fn compounded(extent: SlotExtent, divisor: u32) -> SlotExtent {
    match extent {
        SlotExtent::Full => SlotExtent::Divided { divisor },
        SlotExtent::Divided { divisor: standing } => SlotExtent::Divided { divisor: standing * divisor },
    }
}

impl Graph {
    fn new(canvas_height: usize, divisor: u32) -> Self {
        Self {
            body: SlotExtent::Divided { divisor },
            divisor,
            passes: Vec::new(),
            transients: Vec::new(),
            uniform_bytes: 0,
            canvas_height,
        }
    }

    fn plane(&mut self, extent: SlotExtent) -> u32 {
        self.transients.push(soft_slot(extent));
        (self.transients.len() - 1) as u32
    }

    /// A transient at [`plane_slot`] — the care flood's two hops, and
    /// nothing else.
    fn index_plane(&mut self, extent: SlotExtent) -> u32 {
        self.transients.push(plane_slot(extent));
        (self.transients.len() - 1) as u32
    }

    fn light_hop(&mut self, extent: SlotExtent) -> u32 {
        self.transients.push(light_slot(extent));
        (self.transients.len() - 1) as u32
    }

    fn window(&mut self, bytes: u32) -> u32 {
        let at = self.uniform_bytes;
        self.uniform_bytes += bytes;
        at
    }

    /// The pixels a plane standing at `extent` is tall on this canvas.
    /// An authored radius resolves through it rather than through the
    /// canvas itself, so a chain over the body softens by the same
    /// fraction of the sheet the body covers — which is what the notch
    /// means and what keeps the two extents one picture.
    fn extent_height(&self, extent: SlotExtent) -> usize {
        match extent {
            SlotExtent::Full => self.canvas_height,
            SlotExtent::Divided { divisor } => (self.canvas_height / divisor as usize).max(1),
        }
    }

    /// One full `image::blur` chain into a fresh transient at `extent`,
    /// softening by `radius_pixels` of the reference sheet. The chain
    /// sweeps at whatever extent that radius affords on the plane it runs
    /// over (iamacoffeepot/aether#4437): the wider the softening, the
    /// fewer texels it needs to carry it, and the reduction compounds
    /// with the extent the plane already stands at. Its three iterations
    /// ride one composite kernel per axis (iamacoffeepot/aether#4441), so
    /// the chain is two passes rather than six. Returns the blurred plane
    /// and the plan its window is written from.
    ///
    /// A chain reads through a filtering sampler, so its source has to
    /// stand at the soft plane ([`soft_slot`]). Every transient in this
    /// graph does; a *binding* need not — the one the flow sweeps is the
    /// ink layer's own 32-bit coverage plane — so a binding source is
    /// carried onto one first ([`puddle::soft_carry_pass`]), a pointwise
    /// pass whose cost is a rounding error against the sweeps it makes
    /// pairable.
    fn blur(&mut self, source: InputSlot, radius_pixels: f32, extent: SlotExtent) -> (InputSlot, BlurPlan) {
        let source = match source {
            InputSlot::Binding { .. } => {
                let out = self.plane(extent);
                self.passes.push(puddle::soft_carry_pass(source, OutputSlot::Transient { index: out }));
                transient(out)
            }
            other => other,
        };
        let tuned = image::tuned(radius_pixels, self.extent_height(extent));
        let divisor = puddle::blur_divisor(tuned);
        let plan = BlurPlan {
            window: self.window(puddle::BoxBlurUniforms::BYTES),
            half_width_texels: puddle::box_half_width(tuned, divisor),
            divisor,
        };

        let swept = compounded(extent, divisor);
        let chain = puddle::BoxBlurChain {
            scratch: self.plane(swept),
            carry: self.plane(swept),
            divisor,
            half_width_texels: plan.half_width_texels,
        };
        let out = self.plane(extent);
        self.passes.extend(puddle::box_blur_passes(source, &chain, OutputSlot::Transient { index: out }, plan.window));

        (transient(out), plan)
    }

    fn glue(
        &mut self,
        entry_point: &str,
        inputs: Vec<InputSlot>,
        uniform_offset: u32,
        uniform_length: u32,
        extent: SlotExtent,
    ) -> InputSlot {
        let out = self.plane(extent);
        self.passes.push(ProgramPass {
            stage: PassStage::Fragment,
            entry_point: entry_point.to_owned(),
            inputs,
            output: OutputSlot::Transient { index: out },
            uniform_offset,
            uniform_length,
            repeat: None,
        });
        transient(out)
    }

    /// The drawing's own flow, laid as passes: `image::structure_tensor_flow`
    /// with this graph's blur chain doing the softening and the three
    /// poolings, and [`flow`]'s two pointwise ends between them.
    fn flow_chain(&mut self, ink: InputSlot) -> (FlowPlan, [InputSlot; 3]) {
        let (soft, gradient_blur) = self.blur(ink, image::GRADIENT_BLUR, self.body);

        let mut tensors = Vec::with_capacity(flow::COMPONENTS.len());
        let mut pooled = Vec::with_capacity(flow::COMPONENTS.len());
        for _ in flow::COMPONENTS {
            let select = self.window(flow::SelectUniforms::BYTES);
            let component = self.plane(self.body);
            self.passes.push(flow::tensor_pass(soft, OutputSlot::Transient { index: component }, select));
            let (plane, pool) = self.blur(transient(component), image::TENSOR_BLUR, self.body);
            tensors.push((select, pool));
            pooled.push(plane);
        }
        let pooled = [pooled[0], pooled[1], pooled[2]];

        let mut resolves = Vec::with_capacity(flow::COMPONENTS.len());
        let mut answers = Vec::with_capacity(flow::COMPONENTS.len());
        for _ in flow::COMPONENTS {
            let select = self.window(flow::SelectUniforms::BYTES);
            let answer = self.plane(self.body);
            self.passes.push(flow::resolve_pass(pooled, OutputSlot::Transient { index: answer }, select));
            resolves.push(select);
            answers.push(transient(answer));
        }

        (FlowPlan { gradient_blur, tensors, resolves }, [answers[0], answers[1], answers[2]])
    }

    /// The face paint, laid as passes: the aperture fill and what the two
    /// halves of it become. Returns the iris coverage and the lid weight
    /// at the sheet's own pixels, and the finished cheek flush at the
    /// body's.
    fn face_chain(&mut self) -> (FacePlan, [InputSlot; 3]) {
        let clip_fill = self.plane(FINE);
        self.passes.push(face::aperture_pass(APERTURE_GEOMETRY, OutputSlot::Transient { index: clip_fill }));
        let (clip, clip_blur) = self.blur(transient(clip_fill), accent::CLIP_BLUR, FINE);

        let fine_eyes = self.window(face::FaceUniforms::BYTES);
        let iris = self.plane(FINE);
        self.passes.push(face::clipped_pass(face::IRIS_ENTRY, clip, OutputSlot::Transient { index: iris }, fine_eyes));
        let weight = self.plane(FINE);
        self.passes.push(face::clipped_pass(
            face::LID_WEIGHT_ENTRY,
            clip,
            OutputSlot::Transient { index: weight },
            fine_eyes,
        ));

        // The flush develops at the body's extent, so its frames arrive in
        // the body's own pixels and its skin gate reads the packed plane
        // at the packed plane's own extent — no seam anywhere in the
        // chain. A cheek apple spans four eye-sizes and is softened by
        // eight reference pixels on top; there is nothing in it the
        // sheet's own pixels would carry.
        let body_eyes = self.window(face::FaceUniforms::BYTES);
        let flush = self.plane(self.body);
        self.passes.push(face::flush_pass(OutputSlot::Transient { index: flush }, body_eyes));

        let skin_mask = self.window(MaskUniforms::BYTES);
        let skin = self.glue("fs_mask", vec![binding(PACKED)], skin_mask, MaskUniforms::BYTES, self.body);
        let (under, skin_blur) = self.blur(skin, accent::SKIN_BLUR, self.body);

        let gated = self.plane(self.body);
        self.passes.push(face::gate_pass(
            transient(flush),
            under,
            binding(PACKED),
            OutputSlot::Transient { index: gated },
        ));
        let (blush, flush_blur) = self.blur(transient(gated), accent::FLUSH_BLUR, self.body);

        (
            FacePlan { fine_eyes, body_eyes, clip_blur, skin_mask, skin_blur, flush_blur },
            [transient(iris), transient(weight), blush],
        )
    }

    /// One wash laid as passes: the whole of `Sheet::wash` for one set of
    /// params, at one grain. Structure follows the params alone — pour
    /// count, sag, lost, spatter — never the develop's data.
    fn wash_chain(
        &mut self,
        mask: InputSlot,
        value: Option<InputSlot>,
        params: &WashParams,
        grain: Grain,
    ) -> (InputSlot, ChainPlan) {
        let extent = grain.extent;
        let margin = params.water + field::SUPPORT_MARGIN;
        let support = value.map(|plane| self.blur(plane, margin, extent));
        let reference = value.map(|_| self.blur(mask, margin, extent));
        let (support_value, support_value_blur) = support.map_or((mask, None), |(plane, plan)| (plane, Some(plan)));
        let (support_reference, support_reference_blur) =
            reference.map_or((mask, None), |(plane, plan)| (plane, Some(plan)));

        let mut density: Option<InputSlot> = None;
        let mut pours = Vec::with_capacity(params.pours.len());
        for pour in params.pours {
            // A pour that is neither shrunk nor displaced is skipped by
            // the CPU too; a tight wash never wanders and pours at full
            // size, so its chain omits the resample structurally.
            let placed = (params.wander > 0.0 || (pour.scale - 1.0).abs() >= f32::EPSILON).then(|| {
                let window = self.window(puddle::ShrinkUniforms::BYTES);
                let out = self.plane(extent);
                self.passes.push(puddle::shrink_pass(mask, OutputSlot::Transient { index: out }, window));
                (transient(out), window)
            });
            let (source, shrink) = placed.map_or((mask, None), |(plane, window)| (plane, Some(window)));

            let (blurred, soft_blur) = self.blur(source, params.water, extent);
            let (soft, sag) = if params.sag {
                let window = self.window(pigment::SagUniforms::BYTES);
                let out = self.plane(extent);
                self.passes.push(pigment::sag_pass(blurred, OutputSlot::Transient { index: out }, window));
                (transient(out), Some(window))
            } else {
                (blurred, None)
            };

            let threshold = self.window(puddle::ThresholdUniforms::BYTES);
            let hard = {
                let out = self.plane(extent);
                self.passes.push(puddle::threshold_pass(
                    soft,
                    grain.edge,
                    OutputSlot::Transient { index: out },
                    threshold,
                ));
                transient(out)
            };
            let (alpha, lost) = if params.lost.is_some() {
                let window = self.window(SHEET_PARAMS_BYTES);
                let out = self.plane(extent);
                self.passes.push(lost_edge_pass(hard, soft, OutputSlot::Transient { index: out }, window));
                (transient(out), Some(window))
            } else {
                (hard, None)
            };

            let (interior, interior_blur) =
                self.blur(alpha, params.water * field::RIM_SPREAD + field::SUPPORT_MARGIN, extent);
            let rim = self.window(puddle::RimUniforms::BYTES);
            let rim_plane = {
                let out = self.plane(extent);
                self.passes.push(puddle::rim_pass(
                    alpha,
                    interior,
                    grain.edge,
                    OutputSlot::Transient { index: out },
                    rim,
                ));
                transient(out)
            };

            let accumulate = self.window(AccumulateUniforms::BYTES);
            density = Some(self.glue(
                "fs_pour_accumulate",
                vec![density.unwrap_or(mask), alpha, rim_plane, support_value, support_reference],
                accumulate,
                AccumulateUniforms::BYTES,
                extent,
            ));
            pours.push(PourPlan { shrink, soft_blur, sag, threshold, lost, interior_blur, rim, accumulate });
        }

        let granulate = self.window(pigment::GranulateUniforms::BYTES);
        let poured = density.expect("a wash pours at least once");
        let mut settled = {
            let out = self.plane(extent);
            self.passes.push(pigment::granulate_pass(
                poured,
                grain.tooth,
                OutputSlot::Transient { index: out },
                granulate,
            ));
            transient(out)
        };
        let spatter = (params.spatter > 0).then(|| {
            let window = self.window(pigment::SpatterUniforms::BYTES);
            let out = self.plane(extent);
            self.passes.push(pigment::spatter_pass(settled, OutputSlot::Transient { index: out }, window));
            settled = transient(out);
            window
        });

        (settled, ChainPlan { support_value_blur, support_reference_blur, pours, granulate, spatter })
    }

    /// One coat absorbed into the light accumulator; returns the next
    /// accumulator hop and the coat's window.
    fn absorb(&mut self, light: InputSlot, density: InputSlot, extent: SlotExtent) -> (InputSlot, u32) {
        let window = self.window(SHEET_PARAMS_BYTES);
        let out = self.light_hop(extent);
        self.passes.push(coat_absorb_pass(light, density, OutputSlot::Transient { index: out }, window));
        (transient(out), window)
    }

    /// One entry of the painter's box, laid whole: its coverage, its
    /// value, the two washes the care ramp mixes, and whatever else the
    /// entry names — the hair's smear and glaze and stain, the iris' lid.
    ///
    /// A coat at the body's grain is absorbed into `light` here; the one
    /// at the sheet's own is handed to `fine_coats` instead, to be
    /// absorbed past the seam. Which of the two is [`Grain`]'s to say.
    fn material(
        &mut self,
        material: &palette::Material,
        fields: &Fields,
        light: &mut InputSlot,
        fine_coats: &mut Vec<(InputSlot, u32)>,
    ) -> MaterialPlan {
        let fine = material.class >= palette::META;
        let grain = if fine {
            Grain::accent(self.divisor)
        } else {
            Grain::body(self.body)
        };
        let extent = grain.extent;

        let (mask, mask_window) = if fine {
            (fields.iris, None)
        } else {
            let window = self.window(MaskUniforms::BYTES);
            (self.glue("fs_mask", vec![binding(PACKED)], window, MaskUniforms::BYTES, extent), Some(window))
        };
        let shade = self.window(ShadeUniforms::BYTES);
        let value = self.glue("fs_shade", vec![binding(PACKED)], shade, ShadeUniforms::BYTES, extent);

        let (tight_density, tight) = self.wash_chain(mask, Some(value), &field::held_params(material), grain);
        let (mut density, loose) = match field::freed_params(material) {
            Some(freed) => {
                let (loose_density, plan) = self.wash_chain(mask, Some(value), &freed, grain);
                let mixed = self.plane(extent);
                self.passes.push(care_mix_pass(
                    tight_density,
                    loose_density,
                    fields.care,
                    OutputSlot::Transient { index: mixed },
                ));
                (transient(mixed), Some(plan))
            }
            None => (tight_density, None),
        };

        let smear = (material.class == HAIR).then(|| {
            let window = self.window(pigment::SmearUniforms::BYTES);
            let (scratch, out) = (self.plane(extent), self.plane(extent));
            let [flow_x, flow_y, coherence] = fields.flow;
            let slots = pigment::SmearSlots { density, flow_x, flow_y, coherence };
            self.passes.extend(pigment::smear_passes(&slots, scratch, OutputSlot::Transient { index: out }, window));
            density = transient(out);
            window
        });
        let lift = fine.then(|| {
            let window = self.window(LiftUniforms::BYTES);
            density = self.glue("fs_lift", vec![density, fields.lid_weight], window, LiftUniforms::BYTES, extent);
            window
        });

        let coat = if fine {
            let window = self.window(SHEET_PARAMS_BYTES);
            fine_coats.push((density, window));
            window
        } else {
            let (absorbed, window) = self.absorb(*light, density, extent);
            *light = absorbed;
            window
        };

        let glaze = (material.class == HAIR).then(|| {
            let (glaze_density, plan) = self.wash_chain(mask, None, &field::glaze_wash_params(), grain);
            let (absorbed, coat) = self.absorb(*light, glaze_density, extent);
            *light = absorbed;
            (plan, coat)
        });

        let atmosphere = material.atmosphere.as_ref().map(|policy| {
            let figure_window = self.window(MaskUniforms::BYTES);
            let figure = self.glue("fs_mask", vec![binding(PACKED)], figure_window, MaskUniforms::BYTES, extent);
            let (halo, halo_blur) = self.blur(mask, policy.halo, extent);
            let (standing, standing_blur) = self.blur(figure, field::ATMOSPHERE_FIGURE, extent);
            let spill_window = self.window(SpillUniforms::BYTES);
            let spill =
                self.glue("fs_atmosphere_spill", vec![halo, standing], spill_window, SpillUniforms::BYTES, extent);

            let (stain, chain) = self.wash_chain(spill, None, &field::atmosphere_wash_params(), grain);
            let (absorbed, coat) = self.absorb(*light, stain, extent);
            *light = absorbed;
            AtmospherePlan { figure: figure_window, halo_blur, standing_blur, spill: spill_window, chain, coat }
        });

        MaterialPlan { mask: mask_window, shade, tight, loose, smear, lift, coat, glaze, atmosphere, grain }
    }
}

/// The fields laid before the material loop that every entry's chains
/// read: how closely the hand is held, which way the drawing runs, and
/// the two planes the chart contributes to the iris.
struct Fields {
    care: InputSlot,
    flow: [InputSlot; 3],
    iris: InputSlot,
    lid_weight: InputSlot,
}

/// Lay the whole develop as one register graph, for a canvas
/// `canvas_height` pixels tall. Static across develops by construction:
/// the structure depends only on the palette, the wash constructors and
/// that height, never on a develop's own data — so one laid graph serves
/// every develop of every subject at that canvas, and only a resize
/// re-lays it. The height enters because a blur's extent does
/// (iamacoffeepot/aether#4437): how few texels a softening can be swept
/// on is a question about how many pixels it covers. A blob written by
/// [`WashProgram::seed_uniforms`] belongs to the graph laid for its own
/// sheet.
pub fn program(canvas_height: usize) -> WashProgram {
    program_at(canvas_height, BODY_DIVISOR)
}

/// The same graph, with the notch at a divisor the caller names.
///
/// One is the notch turned off, and that is what the parity scenario lays
/// its first develop at: an un-notched graph resolves every slot to the
/// sheet's own extent, so it can be held against the full-resolution CPU
/// oracle at a budget that accounts for quantization alone. Laying the
/// shipped divisor beside it in the same scenario is then a measurement of
/// what the notch itself costs, rather than an assertion about it.
pub fn program_at(canvas_height: usize, divisor: u32) -> WashProgram {
    let mut graph = Graph::new(canvas_height, divisor);
    let body = graph.body;

    // The flow, solved off where the frame's own ink stands
    // (iamacoffeepot/aether#4451). The plane arrives as a binding rather
    // than as a pass over ribbon geometry: the ink layer's stroke program
    // already rasterizes every curve at twice the canvas with the hidden
    // points collapsed, so the coverage the flow wants is a reduction of
    // that raster and nothing here has to be handed a CPU split of the
    // drawing to rasterize a second time.
    let (flow_plan, [flow_x, flow_y, coherence]) = graph.flow_chain(binding(INK));

    // How closely the hand is held, flooded out of the bake's own class
    // channel rather than chamfered on the CPU.
    let care_window = graph.window(care::UNIFORM_BYTES);
    // The two flood hops carry seed indices and so stand at the wider
    // plane; what the ramp resolves them into is a quantity like any
    // other, and stands where every other quantity does.
    let (care_carry, care_relay) = (graph.index_plane(body), graph.index_plane(body));
    let care_out = graph.plane(body);
    graph.passes.extend(care::passes(
        binding(PACKED),
        care_carry,
        care_relay,
        OutputSlot::Transient { index: care_out },
        care_window,
    ));
    let care = transient(care_out);

    let (face_plan, [iris_mask, lid_weight, blush]) = graph.face_chain();

    let mut light = {
        let out = graph.light_hop(body);
        graph.passes.push(light_prime_pass(OutputSlot::Transient { index: out }));
        transient(out)
    };

    // The one coat that develops at the sheet's own pixels, held back
    // until the accumulator has been lifted across the seam. Absorption
    // is a product, so a coat's place in the order carries no meaning —
    // the palette says so itself — and this is what lets the notch cut
    // the sequence in two.
    let mut fine_coats: Vec<(InputSlot, u32)> = Vec::new();

    let fields = Fields { care, flow: [flow_x, flow_y, coherence], iris: iris_mask, lid_weight };
    let materials = palette::MATERIALS
        .iter()
        .map(|material| graph.material(material, &fields, &mut light, &mut fine_coats))
        .collect();

    let (light, blush_coat) = graph.absorb(light, blush, body);

    // The seam: everything coarse is absorbed, so the accumulator lifts to
    // the sheet's own pixels and the accents land on it there.
    let seam = graph.window(LiftExtentUniforms::BYTES);
    let mut light = {
        let out = graph.light_hop(FINE);
        graph.passes.push(ProgramPass {
            stage: PassStage::Fragment,
            entry_point: "fs_light_lift".to_owned(),
            inputs: vec![light],
            output: OutputSlot::Transient { index: out },
            uniform_offset: seam,
            uniform_length: LiftExtentUniforms::BYTES,
            repeat: None,
        });
        transient(out)
    };
    for (density, window) in fine_coats {
        let out = graph.light_hop(FINE);
        graph.passes.push(coat_absorb_pass(light, density, OutputSlot::Transient { index: out }, window));
        light = transient(out);
    }

    graph.passes.push(paper_composite_pass(light, binding(PAPER_SHADE), OutputSlot::Binding { index: SHEET }));

    let register = ProgramRegister {
        wgsl: module(),
        bindings: vec![
            // The bake's own output, bound here as an input. Its format
            // contract lives with the pass that writes it
            // ([`super::bake::packed_slot`]); what this program adds is
            // where it sits relative to the sheet, which is the notch.
            SlotSpec { format: TextureFormat::Rgba8, extent: body },
            plane_slot(body),
            plane_slot(body),
            plane_slot(FINE),
            plane_slot(FINE),
            plane_slot(FINE),
            sheet_slot(),
            // Where the drawing itself landed, written by the ink
            // layer's own program at exactly this extent
            // ([`super::stroke::ink_plane_slot`]).
            plane_slot(body),
        ],
        transients: graph.transients,
        geometries: vec![face::geometry_slot()],
        depth_transients: Vec::new(),
        passes: graph.passes,
    };

    WashProgram {
        divisor,
        register,
        materials,
        flow: flow_plan,
        face: face_plan,
        care: care_window,
        blush_coat,
        seam,
        uniform_bytes: graph.uniform_bytes,
        canvas_height,
    }
}

/// The uniform blob under construction: windows written at the offsets
/// the graph recorded, over a zeroed base so an unwritten window is a
/// neutralized one.
struct Blob(Vec<u8>);

impl Blob {
    fn window(&mut self, offset: u32, bytes: &[u8]) {
        let at = offset as usize;
        self.0[at..at + bytes.len()].copy_from_slice(bytes);
    }
}

/// Everything one chain's windows are written from.
struct ChainData<'a> {
    params: &'a WashParams,
    /// Whether the wash has any coverage at all — the case the CPU paints
    /// nothing for, encoded here as zeroed strengths with no accidents
    /// drawn.
    covered: bool,
    has_value: bool,
}

/// Which of the box's materials carried coverage when a seed slice was
/// rolled.
///
/// The accident stream skips a wash with no coverage — the CPU oracle's
/// own `Sheet::wash` returns before it rolls — so every later material's
/// accidents depend on which earlier ones were in view. A develop whose
/// visible set has changed therefore has to re-roll rather than re-place,
/// and this is the bit pattern that says so. It changes when a material
/// enters or leaves the frame, which is rare; a centroid that has merely
/// moved does not touch it.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Presence(u16);

impl Presence {
    /// Which materials [`Placement`] puts on the canvas.
    #[must_use]
    pub fn of(placement: &Placement<'_>) -> Self {
        let mut bits = 0u16;
        for (index, material) in palette::MATERIALS.iter().enumerate() {
            if placement.centre_of(material.class).is_some() {
                bits |= 1 << index;
            }
        }

        Self(bits)
    }

    fn covers(self, index: usize) -> bool {
        self.0 & (1 << index) != 0
    }
}

/// Where one develop's paint goes: the two quantities the class plane used
/// to be read for, answered off the subject's own geometry and off the
/// chart ([`survey`]).
pub struct Placement<'a> {
    /// Per-class centroids in *body* texels, indexed by class id.
    pub centroids: &'a [Option<Vec2>; survey::SLOTS],
    /// Where each class' atmosphere stain sits, in the same texels.
    ///
    /// Separate from the region's own centroid because the stain is not
    /// the region: it is the region's halo carried off along the drift
    /// and *cut back wherever the figure stands*, and on a figure that
    /// fills the frame the cut moves the answer much further than the
    /// drift does. Whoever supplies this decides how to answer it —
    /// the develop estimates it off the geometry, the parity scenario
    /// takes it off the oracle's own spill pixels
    /// ([`field::Sheet::atmosphere_spill`]).
    pub stains: &'a [Option<Vec2>; survey::SLOTS],
    /// Where the iris meta-material sits, in the *sheet's* texels — the
    /// chart owns the iris the way the field owns every other region, so
    /// this is measured off the projected eyes rather than off a class.
    pub iris: Option<Vec2>,
}

impl Placement<'_> {
    fn centre_of(&self, class: u8) -> Option<Vec2> {
        if class >= palette::META {
            return self.iris;
        }

        self.centroids.get(usize::from(class)).copied().flatten()
    }
}

/// The chart's eyes as one develop projected them: at the sheet's own
/// pixels for the iris and its lid, at the body's for the cheek flush,
/// and how much of each the viewer can see.
pub struct Faces<'a> {
    pub fine: &'a [Eye],
    pub body: &'a [Eye],
    pub presence: &'a [f32],
}

/// Everything one frame's uniforms carry that the seed slice could not.
pub struct Frame<'a> {
    pub placement: Placement<'a>,
    /// `None` for a subject with no charted face — the accents neutralize
    /// through zeroed counts and a zeroed blush cap.
    pub faces: Option<Faces<'a>>,
}

/// The windows the seed and the canvas fix, rolled once and re-placed per
/// frame.
pub struct SeedUniforms {
    seed: u64,
    canvas: Canvas,
    presence: Presence,
    blob: Vec<u8>,
}

impl SeedUniforms {
    /// Whether this slice can still be re-placed, or has to be re-rolled.
    #[must_use]
    pub fn serves(&self, seed: u64, canvas: Canvas, presence: Presence) -> bool {
        self.seed == seed && self.canvas == canvas && self.presence == presence
    }
}

impl WashProgram {
    /// The register mail: send once to `aether.render`, keep the
    /// `program_id` for every later dispatch.
    pub fn register(&self) -> &ProgramRegister {
        &self.register
    }

    /// The canvas height this graph was laid for. A sheet of any other
    /// height wants its own graph: the blur extents were chosen against
    /// this one.
    #[must_use]
    pub fn canvas_height(&self) -> usize {
        self.canvas_height
    }

    /// Every window the seed and the canvas fix — which is all of them but
    /// the two eye frames and one centroid per chain.
    ///
    /// The same seed rolls the same accidents in the same order as
    /// `Sheet::coats`, and the palette's pigments and caps ride the coat
    /// windows. Every tuned radius was resolved when the graph was laid,
    /// at the extent its own chain develops at, so a chain's window here
    /// is the plan the structure already committed to rather than a
    /// second reading of the same radius. `presence` decides which washes
    /// roll at all, which is why a slice is keyed on it: a material
    /// entering the frame shifts every later material's stream.
    pub fn seed_uniforms(&self, seed: u64, canvas: Canvas, presence: Presence) -> SeedUniforms {
        debug_assert_eq!(canvas.height, self.canvas_height, "a develop must ride the graph laid for its own canvas");
        let (_, body_height) = canvas.body_at(self.divisor);
        let mut blob = Blob(vec![0u8; self.uniform_bytes as usize]);
        let mut rng = Rng::new(seed);

        blob.window(self.seam, &LiftExtentUniforms { source_scale: 1.0 / self.divisor as f32 }.encode());
        care::encode(&mut blob.0, self.care, body_height);
        blob.window(self.flow.gradient_blur.window, &self.flow.gradient_blur.encode());
        for (index, (select, pool)) in self.flow.tensors.iter().enumerate() {
            blob.window(*select, &flow::SelectUniforms { channel: flow::COMPONENTS[index] }.encode());
            blob.window(pool.window, &pool.encode());
        }
        for (index, &select) in self.flow.resolves.iter().enumerate() {
            blob.window(select, &flow::SelectUniforms { channel: flow::COMPONENTS[index] }.encode());
        }

        blob.window(self.face.clip_blur.window, &self.face.clip_blur.encode());
        blob.window(self.face.skin_mask, &MaskUniforms { material_class: SKIN, figure: false }.encode());
        blob.window(self.face.skin_blur.window, &self.face.skin_blur.encode());
        blob.window(self.face.flush_blur.window, &self.face.flush_blur.encode());

        for (index, (material, plan)) in palette::MATERIALS.iter().zip(&self.materials).enumerate() {
            let (width, height) = plan.grain.pixels(canvas, self.divisor);
            let source_scale = plan.grain.source_scale;

            if let Some(window) = plan.mask {
                blob.window(window, &MaskUniforms { material_class: material.class, figure: false }.encode());
            }
            let lit = material.shade_lit.unwrap_or(palette::LIT);
            blob.window(plan.shade, &ShadeUniforms { shade_floor: material.shade_floor, lit, source_scale }.encode());
            blob.window(plan.coat, &CoatParams { pigment: material.pigment, cap: palette::DENSITY_CAP }.encode());

            let covered = presence.covers(index);
            let held = field::held_params(material);
            encode_chain(
                &mut blob,
                &plan.tight,
                &ChainData { params: &held, covered, has_value: true },
                &mut rng,
                width,
                height,
            );
            if let (Some(freed), Some(loose)) = (field::freed_params(material), plan.loose.as_ref()) {
                encode_chain(
                    &mut blob,
                    loose,
                    &ChainData { params: &freed, covered, has_value: true },
                    &mut rng,
                    width,
                    height,
                );
            }

            if let Some(window) = plan.smear {
                blob.window(window, &pigment::SmearUniforms::for_canvas(height).encode());
            }

            if let Some((glaze, coat)) = plan.glaze.as_ref() {
                let glaze_params = field::glaze_wash_params();
                encode_chain(
                    &mut blob,
                    glaze,
                    &ChainData { params: &glaze_params, covered, has_value: false },
                    &mut rng,
                    width,
                    height,
                );
                blob.window(*coat, &CoatParams { pigment: field::GLAZE_PIGMENT, cap: field::GLAZE_CAP }.encode());
            }

            if let (Some(policy), Some(atmosphere)) = (material.atmosphere.as_ref(), plan.atmosphere.as_ref()) {
                blob.window(atmosphere.figure, &MaskUniforms { material_class: 0, figure: true }.encode());
                blob.window(atmosphere.halo_blur.window, &atmosphere.halo_blur.encode());
                blob.window(atmosphere.standing_blur.window, &atmosphere.standing_blur.encode());
                blob.window(atmosphere.spill, &SpillUniforms { drift: policy.carried(height) }.encode());

                let mut air = Rng::new(seed ^ field::ATMOSPHERE_SEED ^ u64::from(material.class));
                let stain_params = field::atmosphere_wash_params();
                let data = ChainData { params: &stain_params, covered, has_value: false };
                encode_chain(&mut blob, &atmosphere.chain, &data, &mut air, width, height);
                blob.window(atmosphere.coat, &CoatParams { pigment: policy.pigment, cap: policy.cap }.encode());
            }
        }

        SeedUniforms { seed, canvas, presence, blob: blob.0 }
    }

    /// One frame's blob: the seed slice re-placed for this view.
    ///
    /// Everything written here turns with the camera and nothing else
    /// does — the two eye frames, the iris lift and blush gates, and the
    /// centroid every wash places its pours, its lost edge and its thrown
    /// drops about. The rest is a memcpy.
    ///
    /// # Panics
    ///
    /// When `seed` was not rolled for this frame's canvas and presence —
    /// the caller re-rolls through [`SeedUniforms::serves`] first, and
    /// placing accidents that were never rolled for this view would paint
    /// a plausible picture with every wash in the wrong place.
    #[must_use]
    pub fn frame_uniforms(&self, seed: &SeedUniforms, frame: &Frame<'_>) -> Vec<u8> {
        assert_eq!(
            seed.presence,
            Presence::of(&frame.placement),
            "the seed slice was rolled for a different set of visible materials",
        );
        let canvas = seed.canvas;
        let (_, body_height) = canvas.body_at(self.divisor);
        let mut blob = Blob(seed.blob.clone());

        let charted = frame.faces.as_ref();
        let fine_eyes = charted.map_or_else(Vec::new, |faces| face::eyes(faces.fine, faces.presence, canvas.height));
        let body_eyes = charted.map_or_else(Vec::new, |faces| face::eyes(faces.body, faces.presence, body_height));
        blob.window(self.face.fine_eyes, &face::FaceUniforms { eyes: &fine_eyes }.encode());
        blob.window(self.face.body_eyes, &face::FaceUniforms { eyes: &body_eyes }.encode());

        let blush_cap = if charted.is_some() {
            palette::BLUSH_CAP
        } else {
            0.0
        };
        blob.window(self.blush_coat, &CoatParams { pigment: palette::BLUSH_PIGMENT, cap: blush_cap }.encode());

        for (material, plan) in palette::MATERIALS.iter().zip(&self.materials) {
            if let Some(window) = plan.lift {
                blob.window(window, &LiftUniforms { gate: f32::from(charted.is_some()) }.encode());
            }

            let centre = frame.placement.centre_of(material.class).unwrap_or(Vec2::new(0.0, 0.0));
            for chain in [Some(&plan.tight), plan.loose.as_ref(), plan.glaze.as_ref().map(|(chain, _)| chain)]
                .into_iter()
                .flatten()
            {
                place(&mut blob.0, chain, centre);
            }

            if let Some(atmosphere) = plan.atmosphere.as_ref() {
                let stain =
                    frame.placement.stains.get(usize::from(material.class)).copied().flatten().unwrap_or(centre);
                place(&mut blob.0, &atmosphere.chain, stain);
            }
        }

        blob.0
    }
}

/// Write one chain's centroid into every window that carries one.
///
/// All three placing ops — the shrink's resample, the lost edge's bearing
/// pole, the spatter's throw — declare the centre as the first two words
/// of their block, so one writer serves them all and nothing else in the
/// window is disturbed.
fn place(blob: &mut [u8], chain: &ChainPlan, centre: Vec2) {
    for window in chain.centre_windows() {
        let at = window as usize;
        blob[at..at + 4].copy_from_slice(&centre.x.to_le_bytes());
        blob[at + 4..at + 8].copy_from_slice(&centre.y.to_le_bytes());
    }
}

/// Write one wash's seed-fixed windows: roll its accidents exactly when
/// the CPU would (a mask with coverage), and zero every strength when it
/// would not, so an absent region deposits nothing anywhere along its
/// chain. The centroid itself is left at the origin for
/// [`WashProgram::frame_uniforms`] to place. The blur windows are the
/// graph's own — radius and extent were settled together when it was
/// laid — so nothing here re-derives them.
fn encode_chain(blob: &mut Blob, plan: &ChainPlan, data: &ChainData<'_>, rng: &mut Rng, width: usize, height: usize) {
    let params = data.params;
    for blur in [plan.support_value_blur.as_ref(), plan.support_reference_blur.as_ref()].into_iter().flatten() {
        blob.window(blur.window, &blur.encode());
    }

    let accidents = data.covered.then(|| field::WashAccidents::roll(params, width, height, rng));
    let origin = Vec2::new(0.0, 0.0);

    for (index, (pour, pour_plan)) in params.pours.iter().zip(&plan.pours).enumerate() {
        let accident = accidents.as_ref().map(|rolled| &rolled.pours[index]);
        if let Some(window) = pour_plan.shrink {
            let jitter = accident.map_or(origin, |accident| accident.jitter);
            blob.window(window, &puddle::ShrinkUniforms { centre: origin, jitter, scale: pour.scale }.encode());
        }
        blob.window(pour_plan.soft_blur.window, &pour_plan.soft_blur.encode());
        if let Some(window) = pour_plan.sag {
            blob.window(window, &pigment::SagUniforms::for_canvas(height).encode());
        }

        let noise_window = accident.map_or((0, 0), |accident| (accident.window.0 as u32, accident.window.1 as u32));
        let threshold = puddle::ThresholdUniforms {
            window: noise_window,
            level: params.level,
            band: field::EDGE_BAND,
            wobble: params.wobble,
        };
        blob.window(pour_plan.threshold, &threshold.encode());
        if let Some(window) = pour_plan.lost {
            let angle = params.lost.expect("a lost window is laid only for a losing wash");
            blob.window(window, &LostEdgeParams { centre: origin, angle }.encode());
        }

        blob.window(pour_plan.interior_blur.window, &pour_plan.interior_blur.encode());
        let strength = if accidents.is_some() {
            params.rim * params.load * field::RIM_GAIN
        } else {
            0.0
        };
        blob.window(pour_plan.rim, &puddle::RimUniforms { window: noise_window, strength }.encode());

        let body_load = if accidents.is_some() {
            pour.body * params.load
        } else {
            0.0
        };
        let accumulate =
            AccumulateUniforms { keep: f32::from(index > 0), body_load, has_value: f32::from(data.has_value) };
        blob.window(pour_plan.accumulate, &accumulate.encode());
    }

    blob.window(plan.granulate, &pigment::GranulateUniforms { gran: params.gran }.encode());
    if let Some(window) = plan.spatter {
        let drops = accidents.as_ref().map_or(&[][..], |rolled| &rolled.drops[..]);
        blob.window(window, &pigment::SpatterUniforms { centre: origin, drops }.encode());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the notch must spare the accents.
    ///
    /// The whole argument for developing coarsely is that a wash is low
    /// frequency and an iris is not — at this framing an iris is a couple
    /// of dozen pixels across and its slit a fraction of one. A notch that
    /// swallowed the iris chain would still produce a complete, plausible
    /// painting, with her eyes as blue smudges behind the ink, and nothing
    /// would error. So the two extents are checked to be genuinely two,
    /// and the iris to be on the fine side of the line.
    #[test]
    fn the_notch_cuts_the_body_and_spares_the_iris() {
        let program = program(1200);
        let fine = program.materials.iter().filter(|plan| plan.grain.fine).count();

        assert_eq!(fine, 1, "exactly one material — the iris — develops at the sheet's own pixels");
        assert!(
            palette::MATERIALS.last().is_some_and(|material| material.class >= palette::META),
            "the fine material must be last in the box, so the seam cuts one contiguous run of coats",
        );
        assert!(
            program.register.transients.iter().any(|slot| slot.extent == SlotExtent::Divided { divisor: BODY_DIVISOR }),
            "the body must genuinely develop at the notched extent",
        );
        assert!(
            program.register.transients.iter().any(|slot| slot.extent == FINE),
            "and the accents at the sheet's own",
        );
    }

    /// Tripwire: every uniform window the graph handed out must fit inside
    /// the blob the encoders fill.
    ///
    /// The layout is bump-allocated while the graph is laid and written
    /// into by offset afterwards, which are two passes over one
    /// arrangement. A window past the end panics on the write; a window
    /// whose declared length overruns the next one corrupts it silently,
    /// and a corrupted radius or strength paints a plausible picture.
    #[test]
    fn the_seed_slice_covers_every_window_the_graph_laid() {
        let canvas = Canvas { width: 120, height: 160 };
        let program = program(canvas.height);
        let seed = program.seed_uniforms(0x5e_ed, canvas, Presence(u16::MAX));

        assert_eq!(seed.blob.len(), program.uniform_bytes as usize);
        for pass in &program.register.passes {
            assert!(
                pass.uniform_offset + pass.uniform_length <= program.uniform_bytes,
                "pass {} windows past the blob's end",
                pass.entry_point,
            );
        }
    }
}
