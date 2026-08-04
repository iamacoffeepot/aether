//! The coat sequencer (iamacoffeepot/aether#4369): the whole of
//! `field::Sheet::coats` plus `palette::composite` composed from the op
//! builders into one registered ADR-0170 program.
//!
//! [`program`] lays the graph once, statically: for every entry in
//! [`palette::MATERIALS`] a coverage mask, a value plane, a tight wash, a
//! loose wash under the care ramp for a material the hand relaxes over,
//! the flow smear for the hair, the lid lift for the iris, the glaze
//! dropped into the wet hair, the atmosphere stain for a material that
//! throws one — each wash's density absorbed into the light accumulator
//! in the same coat order the CPU composites — and the final resolve
//! against paper white into the `Rgba8` sheet binding. Everything that
//! varies per develop rides the uniform blob
//! ([`WashProgram::uniforms`]); a material with no coverage in a given
//! develop is neutralized through zeroed pour and rim strengths, an
//! empty drop list and a zeroed blush cap, never restructured.
//!
//! Chance stays on the CPU: the dispatch encoder replays the exact
//! accident stream the oracle draws — one [`field::WashAccidents::roll`]
//! per wash whose mask has a centroid, in `coats`' own material order,
//! and the atmosphere's own salted stream — so the blob carries the same
//! jitters, noise windows and drops the CPU develop consumes. The CPU
//! wash remains the oracle; `tests/program_wash_scenario.rs` holds the
//! two whole-sheet develops together.

use aether_math::{Mat4, Vec2};
use aether_render::{InputSlot, OutputSlot, PassStage, ProgramPass, ProgramRegister, SlotSpec};

use crate::easel::accent::Accents;
use crate::easel::field::{self, Sheet, WashParams};
use crate::easel::image::{self, Flow, Rng};
use crate::easel::palette;
use crate::labels::HAIR;

use super::sheet::{
    CoatParams, LostEdgeParams, SHEET_PARAMS_BYTES, care_mix_pass, coat_absorb_pass, light_prime_pass, lost_edge_pass,
    paper_composite_pass, sheet_slot,
};
use super::{pigment, puddle};

/// The sequencer's own WGSL: the glue entry points between the op
/// modules. Never registered alone — [`module`] concatenates it after
/// the op modules whose helpers it calls.
pub const WASH_WGSL: &str = include_str!("wash.wgsl");

/// The wash program's one WGSL module: every op module plus the
/// sequencer's glue, concatenated in dependency order.
pub fn module() -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        puddle::PUDDLE_WGSL,
        pigment::PIGMENT_WGSL,
        super::sheet::SHEET_WGSL,
        super::ink::INK_WGSL,
        WASH_WGSL
    )
}

/// The one geometry slot the program declares: the ribbon triangles the
/// ink pass rasterizes, filled by id per dispatch (ADR-0171).
const INK_GEOMETRY: u32 = 0;

/// Dispatch-binding indices, in the order [`program`] declares them and
/// [`WashBindings::to_vec`] lists them.
const CLASSES: u32 = 0;
const TONE: u32 = 1;
const CARE: u32 = 2;
const TOOTH: u32 = 3;
const EDGE: u32 = 4;
const PAPER_SHADE: u32 = 5;
const FLOW_X: u32 = 6;
const FLOW_Y: u32 = 7;
const COHERENCE: u32 = 8;
const LIFT: u32 = 9;
const IRIS: u32 = 10;
const BLUSH: u32 = 11;
const SHEET: u32 = 12;

/// The registry textures one develop dispatches over, named. All are
/// full-extent `R32Float` planes at the canvas size except `sheet`, the
/// writable `Rgba8` the program develops into. A develop without a flow
/// or a chart uploads zero planes for the absent inputs — the uniforms
/// neutralize the passes that would read them.
pub struct WashBindings {
    /// The region class plane, each label as its `f32`.
    pub classes: u32,
    /// How the key light falls ([`field::Planes::tone`]).
    pub tone: u32,
    /// How closely the hand is held ([`Sheet::care`]).
    pub care: u32,
    /// The paper's tooth ([`field::NoisePlanes::tooth`]).
    pub tooth: u32,
    /// The tide-line field ([`field::NoisePlanes::edge`]).
    pub edge: u32,
    /// The sheet's own colour variation ([`Sheet::paper_shade`]).
    pub paper_shade: u32,
    /// The drawing's flow ([`Flow`]), one plane per component.
    pub flow_x: u32,
    pub flow_y: u32,
    pub coherence: u32,
    /// The lid weight over the iris ([`Accents::lift`]).
    pub lift: u32,
    /// The iris meta-material's coverage ([`Accents::mask`]).
    pub iris: u32,
    /// The cheek flush, already a density ([`Accents::blush`]).
    pub blush: u32,
    /// The developed sheet, written by the final composite pass.
    pub sheet: u32,
}

impl WashBindings {
    /// The `ProgramDispatch::bindings` list, in declaration order.
    pub fn to_vec(&self) -> Vec<u32> {
        vec![
            self.classes,
            self.tone,
            self.care,
            self.tooth,
            self.edge,
            self.paper_shade,
            self.flow_x,
            self.flow_y,
            self.coherence,
            self.lift,
            self.iris,
            self.blush,
            self.sheet,
        ]
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
}

impl ShadeUniforms {
    pub const BYTES: u32 = 8;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0u8; Self::BYTES as usize];
        bytes[0..4].copy_from_slice(&self.shade_floor.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.lit.to_le_bytes());
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

/// One pour's uniform windows, in pass order along its chain.
struct PourPlan {
    /// `None` when the chain never resamples (a tight wash's single
    /// full-size pour, which the CPU also skips).
    shrink: Option<u32>,
    soft_blur: u32,
    /// `None` for a wash on a level sheet.
    sag: Option<u32>,
    threshold: u32,
    /// `None` for a wash that holds its whole edge.
    lost: Option<u32>,
    interior_blur: u32,
    rim: u32,
    accumulate: u32,
}

/// One wash's uniform windows: the support blurs, the pours, and the
/// pigment settle.
struct ChainPlan {
    /// `None` for a wash that carries pigment uniformly (no value plane,
    /// so no support blurs at all).
    support_value_blur: Option<u32>,
    support_reference_blur: Option<u32>,
    pours: Vec<PourPlan>,
    granulate: u32,
    /// `None` for a wash that never spatters.
    spatter: Option<u32>,
}

/// The atmosphere stain's windows: the figure mask, the two halo blurs,
/// the displaced spill cut, then a wash like any other.
struct AtmospherePlan {
    figure: u32,
    halo_blur: u32,
    standing_blur: u32,
    spill: u32,
    chain: ChainPlan,
    coat: u32,
}

/// One material's slice of the program: every window its passes read, in
/// the order [`program`] laid them.
struct MaterialPlan {
    /// `None` for a meta-material, whose mask arrives as a binding.
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
}

/// The wash as one registered program: the static graph plus the window
/// layout its dispatch encoder writes into.
pub struct WashProgram {
    register: ProgramRegister,
    materials: Vec<MaterialPlan>,
    blush_coat: u32,
    /// The transient the ink pass bakes the coverage plane into, and the
    /// window its vertex stage reads the camera from.
    ink_plane: u32,
    ink_window: u32,
    uniform_bytes: u32,
}

/// The graph under construction: passes in sequence, one fresh transient
/// per intermediate (the executor pools them by live range), and a
/// bump-allocated uniform layout.
#[derive(Default)]
struct Graph {
    passes: Vec<ProgramPass>,
    transients: Vec<SlotSpec>,
    uniform_bytes: u32,
}

const fn binding(index: u32) -> InputSlot {
    InputSlot::Binding { index }
}

const fn transient(index: u32) -> InputSlot {
    InputSlot::Transient { index }
}

impl Graph {
    fn plane(&mut self) -> u32 {
        self.transients.push(puddle::plane_slot());
        (self.transients.len() - 1) as u32
    }

    fn light_hop(&mut self) -> u32 {
        self.transients.push(sheet_slot());
        (self.transients.len() - 1) as u32
    }

    fn window(&mut self, bytes: u32) -> u32 {
        let at = self.uniform_bytes;
        self.uniform_bytes += bytes;
        at
    }

    /// One full `image::blur` chain into a fresh transient; returns the
    /// blurred plane and its radius window.
    fn blur(&mut self, source: InputSlot) -> (InputSlot, u32) {
        let window = self.window(puddle::BoxBlurUniforms::BYTES);
        let (scratch, carry, out) = (self.plane(), self.plane(), self.plane());
        self.passes.extend(puddle::box_blur_passes(
            source,
            scratch,
            carry,
            OutputSlot::Transient { index: out },
            window,
        ));
        (transient(out), window)
    }

    fn glue(
        &mut self,
        entry_point: &str,
        inputs: Vec<InputSlot>,
        uniform_offset: u32,
        uniform_length: u32,
    ) -> InputSlot {
        let out = self.plane();
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

    /// One wash laid as passes: the whole of `Sheet::wash` for one set of
    /// params. Structure follows the params alone — pour count, sag,
    /// lost, spatter — never the develop's data.
    fn wash_chain(&mut self, mask: InputSlot, value: Option<InputSlot>, params: &WashParams) -> (InputSlot, ChainPlan) {
        let support = value.map(|plane| self.blur(plane));
        let reference = value.map(|_| self.blur(mask));
        let (support_value, support_value_blur) = support.map_or((mask, None), |(plane, window)| (plane, Some(window)));
        let (support_reference, support_reference_blur) =
            reference.map_or((mask, None), |(plane, window)| (plane, Some(window)));

        let mut density: Option<InputSlot> = None;
        let mut pours = Vec::with_capacity(params.pours.len());
        for pour in params.pours {
            // A pour that is neither shrunk nor displaced is skipped by
            // the CPU too; a tight wash never wanders and pours at full
            // size, so its chain omits the resample structurally.
            let placed = (params.wander > 0.0 || (pour.scale - 1.0).abs() >= f32::EPSILON).then(|| {
                let window = self.window(puddle::ShrinkUniforms::BYTES);
                let out = self.plane();
                self.passes.push(puddle::shrink_pass(mask, OutputSlot::Transient { index: out }, window));
                (transient(out), window)
            });
            let (source, shrink) = placed.map_or((mask, None), |(plane, window)| (plane, Some(window)));

            let (blurred, soft_blur) = self.blur(source);
            let (soft, sag) = if params.sag {
                let window = self.window(pigment::SagUniforms::BYTES);
                let out = self.plane();
                self.passes.push(pigment::sag_pass(blurred, OutputSlot::Transient { index: out }, window));
                (transient(out), Some(window))
            } else {
                (blurred, None)
            };

            let threshold = self.window(puddle::ThresholdUniforms::BYTES);
            let hard = {
                let out = self.plane();
                self.passes.push(puddle::threshold_pass(
                    soft,
                    binding(EDGE),
                    OutputSlot::Transient { index: out },
                    threshold,
                ));
                transient(out)
            };
            let (alpha, lost) = if params.lost.is_some() {
                let window = self.window(SHEET_PARAMS_BYTES);
                let out = self.plane();
                self.passes.push(lost_edge_pass(hard, soft, OutputSlot::Transient { index: out }, window));
                (transient(out), Some(window))
            } else {
                (hard, None)
            };

            let (interior, interior_blur) = self.blur(alpha);
            let rim = self.window(puddle::RimUniforms::BYTES);
            let rim_plane = {
                let out = self.plane();
                self.passes.push(puddle::rim_pass(
                    alpha,
                    interior,
                    binding(EDGE),
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
            ));
            pours.push(PourPlan { shrink, soft_blur, sag, threshold, lost, interior_blur, rim, accumulate });
        }

        let granulate = self.window(pigment::GranulateUniforms::BYTES);
        let poured = density.expect("a wash pours at least once");
        let mut settled = {
            let out = self.plane();
            self.passes.push(pigment::granulate_pass(
                poured,
                binding(TOOTH),
                OutputSlot::Transient { index: out },
                granulate,
            ));
            transient(out)
        };
        let spatter = (params.spatter > 0).then(|| {
            let window = self.window(pigment::SpatterUniforms::BYTES);
            let out = self.plane();
            self.passes.push(pigment::spatter_pass(settled, OutputSlot::Transient { index: out }, window));
            settled = transient(out);
            window
        });

        (settled, ChainPlan { support_value_blur, support_reference_blur, pours, granulate, spatter })
    }

    /// One coat absorbed into the light accumulator; returns the next
    /// accumulator hop and the coat's window.
    fn absorb(&mut self, light: InputSlot, density: InputSlot) -> (InputSlot, u32) {
        let window = self.window(SHEET_PARAMS_BYTES);
        let out = self.light_hop();
        self.passes.push(coat_absorb_pass(light, density, OutputSlot::Transient { index: out }, window));
        (transient(out), window)
    }
}

/// Lay the whole develop as one register graph. Static by construction:
/// the structure depends only on the palette and the wash constructors,
/// so it is the same graph for every develop at every canvas size.
pub fn program() -> WashProgram {
    let mut graph = Graph::default();

    // The ink coverage plane, rasterized from resident ribbon geometry
    // (iamacoffeepot/aether#4410). It is laid first so it is written
    // before anything could read it; nothing samples it yet, and
    // iamacoffeepot/aether#4412 is what moves the flow field off the CPU
    // rasterize onto this plane.
    let ink_window = graph.window(super::ink::InkUniforms::BYTES);
    let ink_plane = graph.plane();
    graph.passes.push(super::ink::coverage_pass(INK_GEOMETRY, OutputSlot::Transient { index: ink_plane }, ink_window));

    let mut light = {
        let out = graph.light_hop();
        graph.passes.push(light_prime_pass(OutputSlot::Transient { index: out }));
        transient(out)
    };

    let mut materials = Vec::with_capacity(palette::MATERIALS.len());
    for material in palette::MATERIALS {
        let (mask, mask_window) = if material.class < palette::META {
            let window = graph.window(MaskUniforms::BYTES);
            (graph.glue("fs_mask", vec![binding(CLASSES)], window, MaskUniforms::BYTES), Some(window))
        } else {
            (binding(IRIS), None)
        };
        let shade = graph.window(ShadeUniforms::BYTES);
        let value = graph.glue("fs_shade", vec![binding(TONE)], shade, ShadeUniforms::BYTES);

        let (tight_density, tight) = graph.wash_chain(mask, Some(value), &field::held_params(material));
        let (mut density, loose) = match field::freed_params(material) {
            Some(freed) => {
                let (loose_density, plan) = graph.wash_chain(mask, Some(value), &freed);
                let mixed = graph.plane();
                graph.passes.push(care_mix_pass(
                    tight_density,
                    loose_density,
                    binding(CARE),
                    OutputSlot::Transient { index: mixed },
                ));
                (transient(mixed), Some(plan))
            }
            None => (tight_density, None),
        };

        let smear = (material.class == HAIR).then(|| {
            let window = graph.window(pigment::SmearUniforms::BYTES);
            let scratch = graph.plane();
            let out = graph.plane();
            let slots = pigment::SmearSlots {
                density,
                flow_x: binding(FLOW_X),
                flow_y: binding(FLOW_Y),
                coherence: binding(COHERENCE),
            };
            graph.passes.extend(pigment::smear_passes(&slots, scratch, OutputSlot::Transient { index: out }, window));
            density = transient(out);
            window
        });
        let lift = (material.class == palette::IRIS).then(|| {
            let window = graph.window(LiftUniforms::BYTES);
            density = graph.glue("fs_lift", vec![density, binding(LIFT)], window, LiftUniforms::BYTES);
            window
        });

        let (absorbed, coat) = graph.absorb(light, density);
        light = absorbed;

        let glaze = (material.class == HAIR).then(|| {
            let (glaze_density, plan) = graph.wash_chain(mask, None, &field::glaze_wash_params());
            let (absorbed, coat) = graph.absorb(light, glaze_density);
            light = absorbed;
            (plan, coat)
        });

        let atmosphere = material.atmosphere.as_ref().map(|_| {
            let figure_window = graph.window(MaskUniforms::BYTES);
            let figure = graph.glue("fs_mask", vec![binding(CLASSES)], figure_window, MaskUniforms::BYTES);
            let (halo, halo_blur) = graph.blur(mask);
            let (standing, standing_blur) = graph.blur(figure);
            let spill_window = graph.window(SpillUniforms::BYTES);
            let spill = graph.glue("fs_atmosphere_spill", vec![halo, standing], spill_window, SpillUniforms::BYTES);

            let (stain, chain) = graph.wash_chain(spill, None, &field::atmosphere_wash_params());
            let (absorbed, coat) = graph.absorb(light, stain);
            light = absorbed;
            AtmospherePlan { figure: figure_window, halo_blur, standing_blur, spill: spill_window, chain, coat }
        });

        materials.push(MaterialPlan { mask: mask_window, shade, tight, loose, smear, lift, coat, glaze, atmosphere });
    }

    let (light, blush_coat) = graph.absorb(light, binding(BLUSH));
    graph.passes.push(paper_composite_pass(light, binding(PAPER_SHADE), OutputSlot::Binding { index: SHEET }));

    let mut bindings = vec![puddle::plane_slot(); SHEET as usize];
    bindings.push(sheet_slot());
    let register = ProgramRegister {
        wgsl: module(),
        bindings,
        transients: graph.transients,
        geometries: vec![super::ink::geometry_slot()],
        depth_transients: Vec::new(),
        passes: graph.passes,
    };

    WashProgram { register, materials, blush_coat, ink_plane, ink_window, uniform_bytes: graph.uniform_bytes }
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
    /// The wash's centroid, or `None` for a mask with no coverage — the
    /// case the CPU paints nothing for, encoded here as zeroed strengths
    /// with no accidents drawn.
    centre: Option<Vec2>,
    has_value: bool,
}

impl WashProgram {
    /// The register mail: send once to `aether.render`, keep the
    /// `program_id` for every later dispatch.
    pub fn register(&self) -> &ProgramRegister {
        &self.register
    }

    /// The transient the ink pass bakes its coverage plane into — where a
    /// pass that wants the drawing's own alpha reads it from.
    #[must_use]
    pub fn ink_plane(&self) -> u32 {
        self.ink_plane
    }

    /// The uniform blob for one develop of `sheet` — the dispatch-side
    /// half of [`Sheet::coats`]: the same seed rolls the same accidents
    /// in the same order, every tuned radius is resolved at the sheet's
    /// own height, and the palette's pigments and caps ride the coat
    /// windows. `flow` gates the hair smear exactly as it gates the CPU
    /// one; `accents` gate the iris lift and the blush cap. `view_proj`
    /// is the matrix the ribbons were solved for, which the ink pass'
    /// vertex stage projects them through.
    pub fn uniforms(
        &self,
        sheet: &Sheet<'_>,
        flow: Option<&Flow>,
        accents: Option<&Accents>,
        view_proj: Mat4,
    ) -> Vec<u8> {
        let planes = sheet.planes();
        let (width, height) = (planes.width, planes.height);
        let mut blob = Blob(vec![0u8; self.uniform_bytes as usize]);
        let mut rng = Rng::new(sheet.seed());

        let half_size = Vec2::new(width as f32 * 0.5, height as f32 * 0.5);
        blob.window(self.ink_window, &super::ink::InkUniforms { view_proj, half_size }.encode());

        for (material, plan) in palette::MATERIALS.iter().zip(&self.materials) {
            if let Some(window) = plan.mask {
                blob.window(window, &MaskUniforms { material_class: material.class, figure: false }.encode());
            }
            let lit = material.shade_lit.unwrap_or(palette::LIT);
            blob.window(plan.shade, &ShadeUniforms { shade_floor: material.shade_floor, lit }.encode());
            blob.window(plan.coat, &CoatParams { pigment: material.pigment, cap: palette::DENSITY_CAP }.encode());

            let coverage: Option<Vec<f32>> = if material.class < palette::META {
                Some(palette::mask_of(planes.classes, material.class))
            } else {
                accents.and_then(|accents| accents.mask(material.class)).map(<[f32]>::to_vec)
            };
            let centre = coverage.as_deref().and_then(|mask| field::centroid(mask, width));

            let held = field::held_params(material);
            encode_chain(
                &mut blob,
                &plan.tight,
                &ChainData { params: &held, centre, has_value: true },
                &mut rng,
                width,
                height,
            );
            if let (Some(freed), Some(loose)) = (field::freed_params(material), plan.loose.as_ref()) {
                encode_chain(
                    &mut blob,
                    loose,
                    &ChainData { params: &freed, centre, has_value: true },
                    &mut rng,
                    width,
                    height,
                );
            }

            if let Some(window) = plan.smear {
                let reach = if flow.is_some() {
                    pigment::SmearUniforms::for_canvas(height).reach
                } else {
                    0
                };
                blob.window(window, &pigment::SmearUniforms { reach }.encode());
            }
            if let Some(window) = plan.lift {
                blob.window(window, &LiftUniforms { gate: f32::from(accents.is_some()) }.encode());
            }

            if let Some((glaze, coat)) = plan.glaze.as_ref() {
                let glaze_params = field::glaze_wash_params();
                encode_chain(
                    &mut blob,
                    glaze,
                    &ChainData { params: &glaze_params, centre, has_value: false },
                    &mut rng,
                    width,
                    height,
                );
                blob.window(*coat, &CoatParams { pigment: field::GLAZE_PIGMENT, cap: field::GLAZE_CAP }.encode());
            }

            if let (Some(policy), Some(atmosphere)) = (material.atmosphere.as_ref(), plan.atmosphere.as_ref()) {
                blob.window(atmosphere.figure, &MaskUniforms { material_class: 0, figure: true }.encode());
                blob.window(atmosphere.halo_blur, &blur_radius(policy.halo, height).encode());
                blob.window(atmosphere.standing_blur, &blur_radius(field::ATMOSPHERE_FIGURE, height).encode());
                let drift = Vec2::new(image::tuned(policy.drift.0, height), image::tuned(policy.drift.1, height));
                blob.window(atmosphere.spill, &SpillUniforms { drift }.encode());

                // The stain's centroid comes from the same spill pixels
                // the GPU pass writes, and its accidents from the stain's
                // own salted stream — both exactly as `Sheet::coats`.
                let spill_centre =
                    coverage.as_deref().and_then(|mask| field::centroid(&sheet.atmosphere_spill(mask, policy), width));
                let mut air = Rng::new(sheet.seed() ^ field::ATMOSPHERE_SEED ^ u64::from(material.class));
                let stain_params = field::atmosphere_wash_params();
                let data = ChainData { params: &stain_params, centre: spill_centre, has_value: false };
                encode_chain(&mut blob, &atmosphere.chain, &data, &mut air, width, height);
                blob.window(atmosphere.coat, &CoatParams { pigment: policy.pigment, cap: policy.cap }.encode());
            }
        }

        let blush_cap = if accents.is_some() {
            palette::BLUSH_CAP
        } else {
            0.0
        };
        blob.window(self.blush_coat, &CoatParams { pigment: palette::BLUSH_PIGMENT, cap: blush_cap }.encode());

        blob.0
    }
}

/// A blur window at a reference-sheet radius: tuned to this canvas, then
/// through the same box mapping the CPU blur rounds with.
fn blur_radius(radius_pixels: f32, height: usize) -> puddle::BoxBlurUniforms {
    puddle::BoxBlurUniforms { radius_texels: puddle::box_radius_texels(image::tuned(radius_pixels, height)) }
}

/// Write one wash's windows: roll its accidents exactly when the CPU
/// would (a mask with a centroid), and zero every strength when it would
/// not, so an absent region deposits nothing anywhere along its chain.
fn encode_chain(blob: &mut Blob, plan: &ChainPlan, data: &ChainData<'_>, rng: &mut Rng, width: usize, height: usize) {
    let params = data.params;
    let margin = params.water + field::SUPPORT_MARGIN;
    if let Some(window) = plan.support_value_blur {
        blob.window(window, &blur_radius(margin, height).encode());
    }
    if let Some(window) = plan.support_reference_blur {
        blob.window(window, &blur_radius(margin, height).encode());
    }

    let accidents = data.centre.map(|_| field::WashAccidents::roll(params, width, height, rng));
    let centre = data.centre.unwrap_or(Vec2::new(0.0, 0.0));

    for (index, (pour, pour_plan)) in params.pours.iter().zip(&plan.pours).enumerate() {
        let accident = accidents.as_ref().map(|rolled| &rolled.pours[index]);
        if let Some(window) = pour_plan.shrink {
            let jitter = accident.map_or(Vec2::new(0.0, 0.0), |accident| accident.jitter);
            blob.window(window, &puddle::ShrinkUniforms { centre, jitter, scale: pour.scale }.encode());
        }
        blob.window(pour_plan.soft_blur, &blur_radius(params.water, height).encode());
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
            blob.window(window, &LostEdgeParams { centre, angle }.encode());
        }

        blob.window(
            pour_plan.interior_blur,
            &blur_radius(params.water * field::RIM_SPREAD + field::SUPPORT_MARGIN, height).encode(),
        );
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
        blob.window(window, &pigment::SpatterUniforms { centre, drops }.encode());
    }
}
