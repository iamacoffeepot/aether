//! Register-time program validation (ADR-0170): the WGSL through naga,
//! then the declared graph — pure CPU, no device. Success produces the
//! [`ProgramPlan`] the register path builds pipelines from and the
//! dispatch path records against; failure produces the `Err { reason }`
//! string, one distinguishable message per validation class.

use naga::front::wgsl;
use naga::valid::{Capabilities, ModuleInfo, ValidationFlags, Validator};
use naga::{
    AddressSpace, Binding, BuiltIn, Handle, Module, Scalar, ScalarKind, ShaderStage,
    StorageAccess as NagaStorageAccess, Type, TypeInner, VectorSize,
};

use super::super::surface::render_limits;
use super::draw_sets::validate::{DrawSetsPlan, list_slots, validate_draw_sets};
use crate::{
    Blend, ComputeBufferBinding, ComputePass, DepthExtent, DepthSpec, DrawPass, GeometryBuffer, GeometrySlotSpec,
    InputSlot, Mips, OutputSlot, PassLoad, PassStage, ProgramPass, ProgramRegister, Samples, Sampling, SlotExtent,
    SlotShape, SlotSpec, StorageAccess, TextureFormat, TransientSpec, VertexAttribute, VertexFormat, Wrap,
};

/// Ceiling on one pass's repeat count: a register-time bound so a typo
/// cannot ask the executor to encode an effectively unbounded number of
/// render passes per dispatch. Generous against the named consumer (a
/// wash chain is hundreds of pours).
const MAX_REPEAT_COUNT: u32 = 4096;

/// Ceiling on the render passes one dispatch encodes, summed over every
/// pass's repeat count. [`MAX_REPEAT_COUNT`] bounds a single entry, which
/// leaves the product of many entries unbounded — a graph of a few dozen
/// maximally-repeated passes is a small mail that asks the executor to
/// encode millions of passes and stalls the frame. Generous against the
/// named consumer (a wash chain is hundreds of pours in one pass).
const MAX_PASS_ITERATIONS: u64 = 65_536;

/// Ceiling on the uniform bytes one dispatch stages. `encode_passes`
/// copies every iteration's window into an offset-aligned staging buffer,
/// so the allocation is the sum over passes of
/// `repeat_count * align_up(bound_window_bytes)` — driven by the declared
/// graph, not by the dispatch blob's size, since a zero `uniform_stride`
/// lets 4096 iterations rebind one small window. Unbounded, a 64 KiB mail
/// stages gigabytes and overflows the `u32` offset the bind group takes.
const MAX_UNIFORM_STAGING_BYTES: u64 = 64 << 20;

/// A pass slot with the register-only `PassOutput` alias resolved away:
/// what the executor actually binds or attaches.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ResolvedSlot {
    /// The dispatch binding at this index.
    Binding(u32),
    /// The transient intermediate at this index.
    Transient(u32),
}

/// One validated pass: explicit stage, the blend it declared, entry
/// point, resolved texture slots, uniform window, and flattened repeat
/// (`repeat_count` is 1 for an unrepeated pass). `output` is `None` for
/// a pass with no color output, which declares `Blend::Replace`: a
/// compute pass, or a rasterizing pass that is depth-only.
#[derive(Debug)]
pub struct PassPlan {
    pub entry_point: String,
    pub stage: PassPlanStage,
    pub blend: Blend,
    pub inputs: Vec<ResolvedSlot>,
    pub output: Option<ResolvedSlot>,
    pub uniform_offset: u32,
    pub uniform_length: u32,
    pub repeat_count: u32,
    pub uniform_stride: u32,
}

impl PassPlan {
    /// Whether the pass samples `slot`.
    pub fn reads(&self, slot: ResolvedSlot) -> bool {
        self.inputs.contains(&slot)
    }

    /// Whether the pass attaches `slot` as its color output.
    pub fn writes(&self, slot: ResolvedSlot) -> bool {
        self.output == Some(slot)
    }
}

#[derive(Debug)]
pub enum PassPlanStage {
    Fragment,
    Draw(DrawPlan),
    DrawIndexedIndirect(DrawPlan),
    Compute(ComputePlan),
    DrawSets(DrawSetsPlan),
}

impl PassPlanStage {
    pub fn draw(&self) -> Option<&DrawPlan> {
        match self {
            Self::Draw(draw) | Self::DrawIndexedIndirect(draw) => Some(draw),
            Self::Fragment | Self::Compute(_) | Self::DrawSets(_) => None,
        }
    }

    pub fn compute(&self) -> Option<&ComputePlan> {
        match self {
            Self::Compute(compute) => Some(compute),
            Self::Fragment | Self::Draw(_) | Self::DrawIndexedIndirect(_) | Self::DrawSets(_) => None,
        }
    }

    pub fn draw_sets(&self) -> Option<&DrawSetsPlan> {
        match self {
            Self::DrawSets(draw_sets) => Some(draw_sets),
            Self::Fragment | Self::Draw(_) | Self::DrawIndexedIndirect(_) | Self::Compute(_) => None,
        }
    }

    /// The depth transient a rasterizing stage attaches, whichever
    /// stage declares it: the slot the pool assigns a texture to and
    /// the first-reference clear is sequenced by.
    pub fn depth_slot(&self) -> Option<u32> {
        match self {
            Self::Draw(draw) | Self::DrawIndexedIndirect(draw) => draw.depth,
            Self::DrawSets(draw_sets) => draw_sets.depth.map(|depth| depth.slot),
            Self::Fragment | Self::Compute(_) => None,
        }
    }
}

/// One validated draw pass (ADR-0171): the authored vertex entry, the
/// geometry slot the dispatch fills, the depth slot it clears and tests
/// against (`None` for a pass that does not depth-test), and the color
/// load semantic it declared (`Load` for a depth-only pass).
#[derive(Debug)]
pub struct DrawPlan {
    pub vertex_entry_point: String,
    pub geometry: u32,
    pub depth: Option<u32>,
    pub load: PassLoad,
}

/// One validated compute pass: the resident buffers bound at group 2
/// and the fixed dispatch grid.
#[derive(Debug)]
pub struct ComputePlan {
    pub buffers: Vec<ComputeBufferBinding>,
    pub workgroups: [u32; 3],
}

/// One transient's declaration plus its live range over the pass
/// sequence — `first_write..=last_use`, `None` when no pass references
/// it. The dispatch-time pool assignment reuses a physical texture
/// whose previous holder's `last_use` lies strictly before the next
/// holder's `first_write`.
#[derive(Debug)]
pub struct TransientPlan {
    pub spec: TransientSpec,
    pub first_write: Option<u32>,
    pub last_use: Option<u32>,
}

/// The validated program graph: everything the register path needs to
/// build pipelines and the dispatch path needs to resolve and record.
#[derive(Debug)]
pub struct ProgramPlan {
    pub bindings: Vec<SlotSpec>,
    pub transients: Vec<TransientPlan>,
    /// Declared geometry slots (ADR-0171), in dispatch-supply order.
    pub geometries: Vec<GeometrySlotSpec>,
    /// Declared depth transients (ADR-0171, ADR-0246 decision 9), by
    /// extent and sample count — the format is fixed at `Depth32Float`.
    pub depth_transients: Vec<DepthSpec>,
    pub passes: Vec<PassPlan>,
    /// The dispatch binding the final pass writes — the program's
    /// result texture, whose size is the reference extent.
    pub output_binding: u32,
    /// Deduplicated binding indices any pass writes; each must resolve
    /// to a `Writable` registry texture at dispatch.
    pub written_bindings: Vec<u32>,
    /// How many draw-set lists a dispatch supplies (ADR-0246): one more
    /// than the highest list index a `DrawSets` pass names, every index
    /// below it named by at least one pass.
    pub draw_set_lists: u32,
}

impl ProgramPlan {
    /// The declared format of a resolved slot.
    pub fn slot_format(&self, slot: ResolvedSlot) -> TextureFormat {
        self.slot_spec(slot).format
    }

    /// What a pass that reads or writes a resolved slot is built
    /// against: a binding's declaration, or a transient in the same
    /// terms (`read_as`), so the layout, the sampler choice and the
    /// extent have one code path for both.
    pub fn slot_spec(&self, slot: ResolvedSlot) -> SlotSpec {
        match slot {
            ResolvedSlot::Binding(index) => self.bindings[index as usize],
            ResolvedSlot::Transient(index) => read_as(self.transients[index as usize].spec),
        }
    }

    /// How many samples a resolved slot holds per texel: a binding is a
    /// registry texture and always has one, and a transient has what it
    /// declares.
    pub fn samples(&self, slot: ResolvedSlot) -> Samples {
        match slot {
            ResolvedSlot::Binding(_) => Samples::One,
            ResolvedSlot::Transient(index) => self.transients[index as usize].spec.samples,
        }
    }

    /// How many samples per texel a pass rasterizes at, which is what
    /// its pipeline is built with: its color output's count, or its
    /// depth slot's for a depth-only pass. A compute pass attaches
    /// nothing and answers `One`.
    pub fn pass_samples(&self, pass: &PassPlan) -> Samples {
        let depth_samples = || pass.stage.depth_slot().map(|slot| self.depth_transients[slot as usize].samples);
        pass.output.map(|output| self.samples(output)).or_else(depth_samples).unwrap_or(Samples::One)
    }

    /// The declared extent of a slot a pass writes or the pool
    /// allocates: a pass output or a transient. Validation admits only
    /// a `Target` in either place, so those are the only slots that
    /// may be asked.
    ///
    /// # Panics
    /// Panics for a slot that is not a `Target`, which no output or
    /// transient of a validated plan is.
    pub fn target_extent(&self, slot: ResolvedSlot) -> SlotExtent {
        target_extent(self.slot_spec(slot)).expect("a validated output or transient slot is a Target")
    }
}

/// A transient in the terms a binding is declared in: a target of its
/// extent that a pass reads through a clamping sampler at the base
/// level. Whether that sampler is linear follows from the format, as it
/// does for a binding.
fn read_as(spec: TransientSpec) -> SlotSpec {
    SlotSpec {
        format: spec.format,
        shape: SlotShape::Target(spec.extent),
        sampling: Sampling::Filtered { wrap: Wrap::Clamp, mips: Mips::Base },
    }
}

/// The extent a slot is sized by, or `None` for a shape that takes a
/// texture of its own size.
fn target_extent(spec: SlotSpec) -> Option<SlotExtent> {
    match spec.shape {
        SlotShape::Target(extent) => Some(extent),
        SlotShape::Texture | SlotShape::TextureArray => None,
    }
}

/// Resolve a declared extent against the reference size. Floor
/// division, clamped to at least one texel — the divisor was checked
/// nonzero at register.
pub fn resolve_extent(extent: SlotExtent, reference: (u32, u32)) -> (u32, u32) {
    match extent {
        SlotExtent::Full => reference,
        SlotExtent::Divided { divisor } => ((reference.0 / divisor).max(1), (reference.1 / divisor).max(1)),
    }
}

/// Resolve a depth slot's declared extent against the reference size:
/// an `Output` extent as [`resolve_extent`] does, and a `Fixed` one to
/// its own square whatever the reference is.
pub fn resolve_depth_extent(extent: DepthExtent, reference: (u32, u32)) -> (u32, u32) {
    match extent {
        DepthExtent::Output(extent) => resolve_extent(extent, reference),
        DepthExtent::Fixed { side } => (side, side),
    }
}

/// The divisor a timing row reports for a depth slot's extent: the
/// declared divisor of an `Output` extent (`1` for `Full`), and `1` for
/// a `Fixed` one, which is divided from nothing.
pub fn depth_divisor(extent: DepthExtent) -> u32 {
    match extent {
        DepthExtent::Output(SlotExtent::Divided { divisor }) => divisor,
        DepthExtent::Output(SlotExtent::Full) | DepthExtent::Fixed { .. } => 1,
    }
}

/// Validate a register mail: naga (parse + validation), then the graph.
/// Returns the plan, or the `Err { reason }` string for the first
/// failing check.
pub fn validate(mail: &ProgramRegister) -> Result<ProgramPlan, String> {
    let module =
        wgsl::parse_str(&mail.wgsl).map_err(|error| format!("invalid wgsl: {}", error.emit_to_string(&mail.wgsl)))?;
    let info = Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .map_err(|error| format!("invalid wgsl: {}", error.emit_to_string(&mail.wgsl)))?;

    if mail.passes.is_empty() {
        return Err("program declares no passes".to_owned());
    }
    for (index, spec) in mail.bindings.iter().enumerate() {
        if let Some(extent) = target_extent(*spec) {
            check_extent(extent, || format!("binding {index}"))?;
        }
    }
    for (index, spec) in mail.transients.iter().enumerate() {
        check_extent(spec.extent, || format!("transient {index}"))?;
        let unresolvable = spec.samples == Samples::Four && !spec.format.resolvable();
        if unresolvable {
            return Err(format!(
                "transient {index}: a Four transient is read resolved, and {:?} cannot be resolved — declare it One, \
                 or in a format that resolves",
                spec.format,
            ));
        }
    }
    for (index, spec) in mail.depth_transients.iter().enumerate() {
        check_depth_extent(index, spec.extent)?;
    }
    for (index, slot) in mail.geometries.iter().enumerate() {
        check_geometry_slot(index, slot)?;
    }

    let mut transients: Vec<TransientPlan> =
        mail.transients.iter().map(|spec| TransientPlan { spec: *spec, first_write: None, last_use: None }).collect();
    let mut passes: Vec<PassPlan> = Vec::with_capacity(mail.passes.len());
    let mut written_bindings: Vec<u32> = Vec::new();
    for (index, pass) in mail.passes.iter().enumerate() {
        let context = PassValidation { mail, module: &module, info: &info, earlier: &passes, transients: &transients };
        let plan = validate_pass(&context, index, pass)?;
        let sequence = u32::try_from(index).expect("pass sequence index fits u32");
        if let Some(ResolvedSlot::Transient(transient)) = plan.output {
            let live = &mut transients[transient as usize];
            live.first_write.get_or_insert(sequence);
            live.last_use = Some(sequence);
        }
        for input in &plan.inputs {
            if let ResolvedSlot::Transient(transient) = input {
                transients[*transient as usize].last_use = Some(sequence);
            }
        }
        if let Some(ResolvedSlot::Binding(binding)) = plan.output
            && !written_bindings.contains(&binding)
        {
            written_bindings.push(binding);
        }
        passes.push(plan);
    }

    check_encode_budget(&passes)?;
    let draw_set_lists = list_slots(&passes)?;

    let final_output = passes.last().expect("passes checked non-empty").output;
    let Some(ResolvedSlot::Binding(output_binding)) = final_output else {
        return Err("the final pass must write a dispatch binding (the program's result texture)".to_owned());
    };
    if mail.bindings[output_binding as usize].shape != SlotShape::Target(SlotExtent::Full) {
        return Err(format!(
            "binding {output_binding}: the program's output binding must declare Target(Full) — its texture's size \
             is the reference every other extent scales from",
        ));
    }

    Ok(ProgramPlan {
        bindings: mail.bindings.clone(),
        transients,
        geometries: mail.geometries.clone(),
        depth_transients: mail.depth_transients.clone(),
        passes,
        output_binding,
        written_bindings,
        draw_set_lists,
    })
}

/// What the whole graph costs the executor per dispatch, checked once the
/// per-pass plans are known. Both ceilings are register-time so the cost
/// is refused where it is declared rather than discovered at record time,
/// where the encode is already underway on the driver thread and the only
/// remaining outcomes are a stalled frame, a multi-gigabyte allocation, or
/// an overflowed offset. One bounded walk over the passes.
fn check_encode_budget(passes: &[PassPlan]) -> Result<(), String> {
    let align = u64::from(render_limits().min_uniform_buffer_offset_alignment).max(4);
    let mut iterations: u64 = 0;
    let mut staged_bytes: u64 = 0;
    for pass in passes {
        let bound = u64::from(pass.uniform_length).max(super::MIN_BOUND_UNIFORM_BYTES);
        iterations = iterations.saturating_add(u64::from(pass.repeat_count));
        staged_bytes =
            staged_bytes.saturating_add(u64::from(pass.repeat_count).saturating_mul(bound.next_multiple_of(align)));
    }
    if iterations > MAX_PASS_ITERATIONS {
        return Err(format!(
            "the graph encodes {iterations} pass iterations per dispatch, over the supported maximum \
             {MAX_PASS_ITERATIONS}",
        ));
    }
    if staged_bytes > MAX_UNIFORM_STAGING_BYTES {
        return Err(format!(
            "the graph stages {staged_bytes} uniform bytes per dispatch, over the supported maximum \
             {MAX_UNIFORM_STAGING_BYTES}",
        ));
    }
    Ok(())
}

fn check_extent(extent: SlotExtent, slot: impl Fn() -> String) -> Result<(), String> {
    match extent {
        SlotExtent::Divided { divisor: 0 } => Err(format!("{}: extent divisor must be at least 1", slot())),
        SlotExtent::Full | SlotExtent::Divided { .. } => Ok(()),
    }
}

/// A depth slot's extent must be one a texture can be created at: an
/// `Output` extent as any other, and a `Fixed` side inside the limit
/// set texture creation is checked against, so a side the device would
/// refuse is refused here and not at the first dispatch.
fn check_depth_extent(index: usize, extent: DepthExtent) -> Result<(), String> {
    match extent {
        DepthExtent::Output(extent) => check_extent(extent, || format!("depth transient {index}")),
        DepthExtent::Fixed { side } => {
            let limit = render_limits().max_texture_dimension_2d;
            if (1..=limit).contains(&side) {
                Ok(())
            } else {
                Err(format!(
                    "depth transient {index}: fixed side {side} is outside 1..={limit}, the device limit \
                     max_texture_dimension_2d",
                ))
            }
        }
    }
}

/// A declared geometry slot must be a layout a vertex buffer can be
/// built from: at least one attribute, and no location claimed twice.
/// Both would otherwise surface as an opaque `pipeline creation failed`
/// from wgpu's own attribute validation.
fn check_geometry_slot(index: usize, slot: &GeometrySlotSpec) -> Result<(), String> {
    if slot.layout.is_empty() {
        return Err(format!("geometry slot {index}: layout declares no attributes"));
    }
    for (position, attribute) in slot.layout.iter().enumerate() {
        if slot.layout[..position].iter().any(|earlier| earlier.location == attribute.location) {
            return Err(format!("geometry slot {index}: layout declares location {} twice", attribute.location));
        }
    }
    Ok(())
}

/// The register-wide context one pass validates against: the mail, its
/// parsed and validated module, the plans of the passes before it, and
/// the transients' liveness so far.
struct PassValidation<'a> {
    mail: &'a ProgramRegister,
    module: &'a Module,
    info: &'a ModuleInfo,
    earlier: &'a [PassPlan],
    transients: &'a [TransientPlan],
}

// One linear walk per pass — entry point, inputs, output, window,
// repeat — reads better in sequence than split into per-check helpers
// that would each re-thread the same pass context.
#[allow(clippy::too_many_lines)] // aether-suppression-request: pre-existing; the attribute only lost its argument-count lint
fn validate_pass(context: &PassValidation<'_>, index: usize, pass: &ProgramPass) -> Result<PassPlan, String> {
    let &PassValidation { mail, module, info, earlier, .. } = context;
    let entry_stage = if matches!(&pass.stage, PassStage::Compute(_)) {
        ShaderStage::Compute
    } else {
        ShaderStage::Fragment
    };
    let entry_stage_name = if entry_stage == ShaderStage::Compute {
        "compute"
    } else {
        "fragment"
    };
    let entry_index = module
        .entry_points
        .iter()
        .position(|entry| entry.stage == entry_stage && entry.name == pass.entry_point)
        .ok_or_else(|| {
            format!("pass {index}: no {entry_stage_name} entry point named `{}` in the module", pass.entry_point)
        })?;

    let mut inputs = Vec::with_capacity(pass.inputs.len());
    for (input_index, input) in pass.inputs.iter().enumerate() {
        inputs.push(resolve_input(context, index, &pass.stage, input_index, *input)?);
    }
    let output = match pass.output {
        OutputSlot::Binding { index: binding } => {
            check_binding_index(mail, index, binding)?;
            let shape = mail.bindings[binding as usize].shape;
            if !matches!(shape, SlotShape::Target(_)) {
                return Err(format!(
                    "pass {index}: binding {binding} is declared {shape:?}, which is read only — a pass writes only \
                     a Target binding",
                ));
            }
            Some(ResolvedSlot::Binding(binding))
        }
        OutputSlot::Transient { index: transient } => {
            check_transient_index(mail, index, transient)?;
            Some(ResolvedSlot::Transient(transient))
        }
        OutputSlot::None => None,
    };
    if output.is_some_and(|output| inputs.contains(&output)) {
        return Err(format!("pass {index} reads its own output slot"));
    }

    let raster = RasterPass {
        index,
        fragment_entry_index: entry_index,
        fragment_entry_point: &pass.entry_point,
        blend: pass.blend,
        attached: attached_output(mail, output),
    };
    let (stage, vertex_entry_index) = match &pass.stage {
        PassStage::Fragment => {
            if output.is_none() {
                return Err(format!("pass {index}: a fragment pass must declare a texture output"));
            }
            (PassPlanStage::Fragment, None)
        }
        PassStage::Draw(draw) | PassStage::DrawIndexedIndirect(draw) => {
            let validated = validate_draw(mail, module, draw, &raster)?;
            if matches!(&pass.stage, PassStage::DrawIndexedIndirect(_)) {
                check_indirect_writer(earlier, index, draw.geometry)?;
            }
            let vertex_entry_index = validated.vertex_entry_index;
            let stage = if matches!(&pass.stage, PassStage::DrawIndexedIndirect(_)) {
                PassPlanStage::DrawIndexedIndirect(validated.plan)
            } else {
                PassPlanStage::Draw(validated.plan)
            };
            (stage, Some(vertex_entry_index))
        }
        PassStage::Compute(compute) => {
            if output.is_some() {
                return Err(format!("pass {index}: a compute pass must declare OutputSlot::None"));
            }
            if pass.blend != Blend::Replace {
                return Err(format!(
                    "pass {index}: a compute pass has no color output to blend onto, so it declares Blend::Replace, \
                     not {:?}",
                    pass.blend,
                ));
            }
            (PassPlanStage::Compute(validate_compute(mail, module, info, index, entry_index, compute)?), None)
        }
        PassStage::DrawSets(draw_sets) => {
            let validated = validate_draw_sets(mail, module, draw_sets, &raster)?;
            (PassPlanStage::DrawSets(validated.plan), Some(validated.vertex_entry_index))
        }
    };

    // The window must cover whichever stages read the block: a draw
    // pass's vertex stage binds the same group-0 window its fragment
    // stage does.
    let block_bytes = [Some(entry_index), vertex_entry_index]
        .into_iter()
        .flatten()
        .filter_map(|entry| uniform_block_bytes(module, info, entry))
        .max();
    if let Some(block_bytes) = block_bytes
        && pass.uniform_length < block_bytes
    {
        return Err(format!(
            "pass {index}: uniform window ({} bytes) is shorter than the shader's uniform block ({block_bytes} bytes)",
            pass.uniform_length,
        ));
    }

    let (repeat_count, uniform_stride) = match pass.repeat {
        None => (1, 0),
        Some(repeat) if repeat.count == 0 => {
            return Err(format!("pass {index}: repeat count must be at least 1"));
        }
        Some(repeat) if repeat.count > MAX_REPEAT_COUNT => {
            return Err(format!(
                "pass {index}: repeat count {} exceeds the supported maximum {MAX_REPEAT_COUNT}",
                repeat.count,
            ));
        }
        Some(repeat) => (repeat.count, repeat.uniform_stride),
    };

    Ok(PassPlan {
        entry_point: pass.entry_point.clone(),
        stage,
        blend: pass.blend,
        inputs,
        output,
        uniform_offset: pass.uniform_offset,
        uniform_length: pass.uniform_length,
        repeat_count,
        uniform_stride,
    })
}

/// The extent and sample count of the color output a rasterizing pass
/// attaches, both of which its depth attachment has to share.
#[derive(Copy, Clone)]
pub struct Attached {
    pub extent: SlotExtent,
    pub samples: Samples,
}

/// What the color output of a pass attaches as: a binding at its
/// declared extent and one sample, a transient as it declares, and
/// nothing for a pass with no color output, which on a rasterizing
/// stage is a depth-only pass.
fn attached_output(mail: &ProgramRegister, output: Option<ResolvedSlot>) -> Option<Attached> {
    let attached = match output? {
        ResolvedSlot::Binding(binding) => Attached {
            extent: target_extent(mail.bindings[binding as usize]).expect("a written binding was checked a Target"),
            samples: Samples::One,
        },
        ResolvedSlot::Transient(transient) => {
            let spec = mail.transients[transient as usize];
            Attached { extent: spec.extent, samples: spec.samples }
        }
    };
    Some(attached)
}

/// What the validation of a rasterizing stage reads from the pass that
/// declares it: its index, its fragment entry point by naga index and
/// by name, its blend, and the color output it attaches (`None` for a
/// depth-only pass).
pub struct RasterPass<'a> {
    pub index: usize,
    pub fragment_entry_index: usize,
    pub fragment_entry_point: &'a str,
    pub blend: Blend,
    pub attached: Option<Attached>,
}

/// A validated draw declaration plus the naga entry index of its vertex
/// stage, which the caller needs for the shared uniform-window check.
struct ValidatedDraw {
    plan: DrawPlan,
    vertex_entry_index: usize,
}

/// Validate the compute-only declaration and its group-2 shader
/// interface. Every declared buffer maps exactly to the same numbered
/// storage binding used by this entry point; leaving the comparison to
/// pipeline creation would collapse access and binding mistakes into an
/// opaque compiler error.
fn validate_compute(
    mail: &ProgramRegister,
    module: &Module,
    info: &ModuleInfo,
    index: usize,
    entry_index: usize,
    compute: &ComputePass,
) -> Result<ComputePlan, String> {
    let limits = render_limits();
    if compute.buffers.len() > limits.max_storage_buffers_per_shader_stage as usize {
        return Err(format!(
            "pass {index}: compute declares {} storage buffers, over the supported maximum {}",
            compute.buffers.len(),
            limits.max_storage_buffers_per_shader_stage,
        ));
    }
    for (dimension, &workgroups) in compute.workgroups.iter().enumerate() {
        if workgroups == 0 {
            return Err(format!("pass {index}: compute workgroup dimension {dimension} must be at least 1"));
        }
        if workgroups > limits.max_compute_workgroups_per_dimension {
            return Err(format!(
                "pass {index}: compute workgroup dimension {dimension} is {workgroups}, over the supported maximum {}",
                limits.max_compute_workgroups_per_dimension,
            ));
        }
    }

    for (binding, declared) in compute.buffers.iter().enumerate() {
        if declared.geometry as usize >= mail.geometries.len() {
            return Err(format!(
                "pass {index}: compute buffer binding {binding} names geometry slot {}, which is out of range ({} declared)",
                declared.geometry,
                mail.geometries.len(),
            ));
        }
        if compute.buffers[..binding]
            .iter()
            .any(|earlier| earlier.geometry == declared.geometry && earlier.buffer == declared.buffer)
        {
            return Err(format!(
                "pass {index}: compute binds geometry slot {} {:?} more than once",
                declared.geometry, declared.buffer,
            ));
        }
    }

    let entry_info = info.get_entry_point(entry_index);
    let mut reflected = Vec::new();
    for (handle, global) in module.global_variables.iter() {
        let Some(binding) = &global.binding else {
            continue;
        };
        if binding.group != 2 || entry_info[handle].is_empty() {
            continue;
        }
        let AddressSpace::Storage { access } = global.space else {
            return Err(format!(
                "pass {index}: compute @group(2) @binding({}) is not a storage buffer",
                binding.binding,
            ));
        };
        reflected.push((binding.binding, access));
    }

    for (binding, declared) in compute.buffers.iter().enumerate() {
        let binding = u32::try_from(binding).expect("compute binding index fits u32");
        let Some((_, access)) = reflected.iter().find(|(reflected, _)| *reflected == binding) else {
            return Err(format!(
                "pass {index}: compute buffer binding {binding} has no used @group(2) @binding({binding}) storage variable",
            ));
        };
        let expected = match declared.access {
            StorageAccess::Read => NagaStorageAccess::LOAD,
            StorageAccess::ReadWrite => NagaStorageAccess::LOAD | NagaStorageAccess::STORE,
        };
        if *access != expected {
            return Err(format!(
                "pass {index}: compute @group(2) @binding({binding}) access disagrees with declared {:?}",
                declared.access,
            ));
        }
    }
    if let Some((binding, _)) = reflected.iter().find(|(binding, _)| *binding as usize >= compute.buffers.len()) {
        return Err(format!(
            "pass {index}: compute shader uses @group(2) @binding({binding}), but only {} buffers are declared",
            compute.buffers.len(),
        ));
    }

    Ok(ComputePlan { buffers: compute.buffers.clone(), workgroups: compute.workgroups })
}

/// An indirect draw may only consume a control buffer a preceding
/// compute pass in this graph declared writable. The control block is
/// seeded with a zero draw count, but rejecting the missing dependency
/// at register makes the intended ordering explicit and testable.
fn check_indirect_writer(earlier: &[PassPlan], index: usize, geometry: u32) -> Result<(), String> {
    let written = earlier.iter().any(|pass| {
        pass.stage.compute().is_some_and(|compute| {
            compute.buffers.iter().any(|binding| {
                binding.geometry == geometry
                    && binding.buffer == GeometryBuffer::DrawIndexedIndirect
                    && binding.access == StorageAccess::ReadWrite
            })
        })
    });
    if written {
        Ok(())
    } else {
        Err(format!(
            "pass {index}: indexed-indirect draw of geometry slot {geometry} has no preceding compute pass that writes its indirect buffer",
        ))
    }
}

/// The `PassStage::Draw` half of pass validation (ADR-0171), in check
/// order: the vertex entry exists, the geometry slot the dispatch fills
/// is declared, the vertex stage's interface agrees with that slot's
/// layout, the depth declaration is coherent with the pass
/// ([`check_depth`]), and a pass with no color output is a well-formed
/// depth-only pass ([`check_depth_only`]).
fn validate_draw(
    mail: &ProgramRegister,
    module: &Module,
    draw: &DrawPass,
    pass: &RasterPass<'_>,
) -> Result<ValidatedDraw, String> {
    let index = pass.index;
    let vertex_entry_index = vertex_entry(module, index, &draw.vertex_entry_point)?;

    let slot = mail.geometries.get(draw.geometry as usize).ok_or_else(|| {
        format!("pass {index}: geometry slot {} is out of range ({} declared)", draw.geometry, mail.geometries.len())
    })?;
    let declared_by = format!("geometry slot {}'s layout", draw.geometry);
    check_vertex_interface(module, index, vertex_entry_index, &slot.layout, &declared_by)?;
    check_depth(mail, module, draw.depth, pass)?;
    check_depth_only(module, draw.load, pass)?;

    Ok(ValidatedDraw {
        plan: DrawPlan {
            vertex_entry_point: draw.vertex_entry_point.clone(),
            geometry: draw.geometry,
            depth: draw.depth,
            load: draw.load,
        },
        vertex_entry_index,
    })
}

/// The naga index of the vertex entry point a rasterizing pass names.
pub(super) fn vertex_entry(module: &Module, index: usize, name: &str) -> Result<usize, String> {
    module
        .entry_points
        .iter()
        .position(|entry| entry.stage == ShaderStage::Vertex && entry.name == name)
        .ok_or_else(|| format!("pass {index}: no vertex entry point named `{name}` in the module"))
}

/// The depth rule every rasterizing pass shares, in its two halves
/// (ADR-0246 decision 9).
///
/// A pass with a color output depth-tests exactly when it names a depth
/// slot, so the ways to be wrong are naming a slot that does not exist,
/// naming a `Fixed` one (the output's size is the size of the texture a
/// dispatch binds, so whether the two agree is not known here), naming
/// one that does not share the color output's extent or its sample
/// count (wgpu requires the attachments of one pass to agree on both),
/// and writing `@builtin(frag_depth)` from the fragment stage with no
/// depth attachment to write it into.
///
/// A pass with no color output is depth-only: its depth slot is all it
/// attaches, so it must name one, and the slot may be of either extent
/// and either sample count.
pub(super) fn check_depth(
    mail: &ProgramRegister,
    module: &Module,
    depth: Option<u32>,
    pass: &RasterPass<'_>,
) -> Result<(), String> {
    let &RasterPass { index, fragment_entry_index, fragment_entry_point, attached, .. } = pass;
    let Some(depth) = depth else {
        if attached.is_none() {
            return Err(format!(
                "pass {index}: a rasterizing pass with OutputSlot::None is depth-only, so it must name a depth \
                 transient — with neither it would write nothing",
            ));
        }
        if writes_frag_depth(module, fragment_entry_index) {
            return Err(format!(
                "pass {index}: entry point `{fragment_entry_point}` writes @builtin(frag_depth), so the pass must \
                 declare a depth transient to write it into",
            ));
        }
        return Ok(());
    };

    let slot = *mail.depth_transients.get(depth as usize).ok_or_else(|| {
        format!("pass {index}: depth transient {depth} is out of range ({} declared)", mail.depth_transients.len())
    })?;
    let Some(attached) = attached else {
        return Ok(());
    };
    let extent = match slot.extent {
        DepthExtent::Output(extent) => extent,
        DepthExtent::Fixed { side } => {
            return Err(format!(
                "pass {index}: depth transient {depth} declares a fixed side {side}, but the pass has a color \
                 output, whose size is not known at register — a Fixed depth slot attaches only under a depth-only \
                 pass",
            ));
        }
    };
    if extent != attached.extent {
        return Err(format!(
            "pass {index}: depth transient {depth} declares extent {extent:?}, which does not match its color \
             output's extent {:?} — a depth attachment must be the size of the color attachment it tests for",
            attached.extent,
        ));
    }
    if slot.samples != attached.samples {
        return Err(format!(
            "pass {index}: depth transient {depth} declares samples {:?}, which does not match its color output's \
             samples {:?} — the attachments of one pass share one sample count",
            slot.samples, attached.samples,
        ));
    }
    Ok(())
}

/// What a rasterizing pass with no color output must also declare
/// (ADR-0246 decision 9), checked after [`check_depth`] has made sure
/// it names a depth slot. `Blend` and `PassLoad` both describe a color
/// output the pass does not have, so any value but the neutral one is
/// refused, as a compute pass's blend is; and the fragment entry point
/// returns no color, since the pipeline has no color target to take
/// one. A pass with a color output passes untouched.
pub(super) fn check_depth_only(module: &Module, load: PassLoad, pass: &RasterPass<'_>) -> Result<(), String> {
    let &RasterPass { index, fragment_entry_index, fragment_entry_point, blend, attached } = pass;
    if attached.is_some() {
        return Ok(());
    }

    if blend != Blend::Replace {
        return Err(format!(
            "pass {index}: a depth-only pass has no color output to blend onto, so it declares Blend::Replace, not \
             {blend:?}",
        ));
    }
    if load != PassLoad::Load {
        return Err(format!(
            "pass {index}: a depth-only pass has no color output to clear, so it declares PassLoad::Load, not \
             {load:?}",
        ));
    }
    if !returns_no_color(module, fragment_entry_index) {
        return Err(format!(
            "pass {index}: entry point `{fragment_entry_point}` returns a color, but a depth-only pass has no \
             color target — its fragment entry point returns nothing or @builtin(frag_depth) alone",
        ));
    }
    Ok(())
}

/// Check the vertex stage's declared interface against the attributes
/// its vertex buffers supply, through naga's reflection: every
/// `@location` the stage reads must be declared by `attributes`, and its
/// WGSL type must be the one that location's format is consumed as. An
/// attribute the stage ignores is fine — a vertex buffer supplies it,
/// and nothing reads it. `declared_by` names where the attributes come
/// from, for the refusal.
pub(super) fn check_vertex_interface(
    module: &Module,
    index: usize,
    vertex_entry_index: usize,
    attributes: &[VertexAttribute],
    declared_by: &str,
) -> Result<(), String> {
    for (location, ty) in entry_input_locations(module, vertex_entry_index) {
        let Some(attribute) = attributes.iter().find(|attribute| attribute.location == location) else {
            return Err(format!(
                "pass {index}: the vertex stage reads @location({location}), which {declared_by} does not declare",
            ));
        };
        if !consumes_format(&module.types[ty].inner, attribute.format) {
            return Err(format!(
                "pass {index}: the vertex stage reads @location({location}) as {}, but {declared_by} declares it \
                 {:?}, which is consumed as {}",
                describe_type(module, ty),
                attribute.format,
                wgsl_type_name(attribute.format),
            ));
        }
    }
    Ok(())
}

/// Every `@location` binding an entry point's arguments carry, flattened
/// through a struct argument's members. Built-in inputs (a vertex index,
/// a fragment position) carry no location and are skipped — they come
/// from the pipeline, not from a vertex buffer.
fn entry_input_locations(module: &Module, entry_index: usize) -> Vec<(u32, Handle<Type>)> {
    let mut locations = Vec::new();
    for argument in &module.entry_points[entry_index].function.arguments {
        match &argument.binding {
            Some(Binding::Location { location, .. }) => locations.push((*location, argument.ty)),
            Some(_) => {}
            None => {
                if let TypeInner::Struct { members, .. } = &module.types[argument.ty].inner {
                    for member in members {
                        if let Some(Binding::Location { location, .. }) = &member.binding {
                            locations.push((*location, member.ty));
                        }
                    }
                }
            }
        }
    }
    locations
}

/// Whether a fragment entry point writes `@builtin(frag_depth)`, either
/// as its whole result or as one member of a struct result.
fn writes_frag_depth(module: &Module, entry_index: usize) -> bool {
    let Some(result) = &module.entry_points[entry_index].function.result else {
        return false;
    };
    if let Some(binding) = &result.binding {
        return is_frag_depth(binding);
    }
    match &module.types[result.ty].inner {
        TypeInner::Struct { members, .. } => {
            members.iter().any(|member| member.binding.as_ref().is_some_and(is_frag_depth))
        }
        _ => false,
    }
}

/// Whether a fragment entry point returns nothing a color target would
/// take: it has no result, its result is `@builtin(frag_depth)`, or its
/// result is a struct whose every member is that builtin.
fn returns_no_color(module: &Module, entry_index: usize) -> bool {
    let Some(result) = &module.entry_points[entry_index].function.result else {
        return true;
    };
    if let Some(binding) = &result.binding {
        return is_frag_depth(binding);
    }
    match &module.types[result.ty].inner {
        TypeInner::Struct { members, .. } => {
            members.iter().all(|member| member.binding.as_ref().is_some_and(is_frag_depth))
        }
        _ => false,
    }
}

fn is_frag_depth(binding: &Binding) -> bool {
    matches!(binding, Binding::BuiltIn(BuiltIn::FragDepth))
}

/// Whether a WGSL type is the one a declared attribute format is
/// consumed as. The integer formats arrive as integers and the
/// normalized ones as floats — the hardware conversion is part of the
/// format, so the shape the shader must declare is fixed per format.
fn consumes_format(inner: &TypeInner, format: VertexFormat) -> bool {
    let (vector_size, kind) = expected_shape(format);
    match (inner, vector_size) {
        (TypeInner::Scalar(scalar), None) => scalar.kind == kind && scalar.width == 4,
        (TypeInner::Vector { size, scalar }, Some(expected)) => {
            *size == expected && scalar.kind == kind && scalar.width == 4
        }
        _ => false,
    }
}

/// The vector width (`None` for a scalar) and scalar kind one declared
/// attribute format is consumed as.
fn expected_shape(format: VertexFormat) -> (Option<VectorSize>, ScalarKind) {
    match format {
        VertexFormat::Float32 => (None, ScalarKind::Float),
        VertexFormat::Float32x2 => (Some(VectorSize::Bi), ScalarKind::Float),
        VertexFormat::Float32x3 => (Some(VectorSize::Tri), ScalarKind::Float),
        VertexFormat::Unorm8x4 => (Some(VectorSize::Quad), ScalarKind::Float),
        VertexFormat::Uint8x4 => (Some(VectorSize::Quad), ScalarKind::Uint),
    }
}

/// The WGSL spelling of the type a declared attribute format is
/// consumed as — what a rejected register tells the author to write.
fn wgsl_type_name(format: VertexFormat) -> &'static str {
    match format {
        VertexFormat::Float32 => "f32",
        VertexFormat::Float32x2 => "vec2<f32>",
        VertexFormat::Float32x3 => "vec3<f32>",
        VertexFormat::Unorm8x4 => "vec4<f32>",
        VertexFormat::Uint8x4 => "vec4<u32>",
    }
}

/// A WGSL-shaped rendering of a naga type, for the mismatch message.
/// Anything that is neither a scalar nor a vector cannot be an
/// attribute, so its declared name (or a stand-in) is enough to name
/// what the author wrote.
fn describe_type(module: &Module, ty: Handle<Type>) -> String {
    match &module.types[ty].inner {
        TypeInner::Scalar(scalar) => scalar_name(*scalar),
        TypeInner::Vector { size, scalar } => format!("vec{}<{}>", *size as u8, scalar_name(*scalar)),
        _ => module.types[ty].name.clone().unwrap_or_else(|| "a non-attribute type".to_owned()),
    }
}

fn scalar_name(scalar: Scalar) -> String {
    let prefix = match scalar.kind {
        ScalarKind::Sint => "i",
        ScalarKind::Uint => "u",
        ScalarKind::Float => "f",
        ScalarKind::Bool => return "bool".to_owned(),
        ScalarKind::AbstractInt | ScalarKind::AbstractFloat => return "an abstract numeric type".to_owned(),
    };
    format!("{prefix}{}", u32::from(scalar.width) * 8)
}

/// Resolve one declared input of the pass at `index`, whose stage is
/// `stage`, to the slot it binds. `InputSlot::Depth` resolves to
/// nothing yet: see [`depth_input_refusal`].
fn resolve_input(
    context: &PassValidation<'_>,
    index: usize,
    stage: &PassStage,
    input_index: usize,
    input: InputSlot,
) -> Result<ResolvedSlot, String> {
    let &PassValidation { mail, earlier, transients, .. } = context;
    match input {
        InputSlot::Binding { index: binding } => {
            check_binding_index(mail, index, binding)?;
            Ok(ResolvedSlot::Binding(binding))
        }
        InputSlot::PassOutput { pass } => earlier
            .get(pass as usize)
            .ok_or_else(|| format!("pass {index} reads the output of pass {pass}, which does not run before it"))?
            .output
            .ok_or_else(|| {
                format!(
                    "pass {index} reads pass {pass} through PassOutput, but that pass has no texture output — a \
                     compute pass and a depth-only pass write none"
                )
            }),
        InputSlot::Transient { index: transient } => {
            check_transient_index(mail, index, transient)?;
            if transients[transient as usize].first_write.is_none() {
                return Err(format!(
                    "pass {index} input {input_index} reads transient {transient} before any earlier pass writes it",
                ));
            }
            Ok(ResolvedSlot::Transient(transient))
        }
        InputSlot::Depth { index: slot, .. } => {
            let attached_here = declared_depth_slot(stage);
            Err(depth_input_refusal(context, index, input_index, slot, attached_here))
        }
    }
}

/// The depth slot a pass's stage declares it attaches, read from the
/// mail: what [`PassPlanStage::depth_slot`] answers for a validated
/// pass, for the pass still being validated.
fn declared_depth_slot(stage: &PassStage) -> Option<u32> {
    match stage {
        PassStage::Draw(draw) | PassStage::DrawIndexedIndirect(draw) => draw.depth,
        PassStage::DrawSets(draw_sets) => draw_sets.depth.map(|depth| depth.slot),
        PassStage::Fragment | PassStage::Compute(_) => None,
    }
}

/// Why a pass may not read depth slot `slot` (ADR-0246 decision 9), in
/// check order: the slot does not exist; it is `Four`, which can be
/// neither compared nor sampled; the pass reading it also attaches it
/// (`attached_here`), which the device refuses; or no earlier pass
/// attaches it, so it would hold nothing. A depth input that passes all
/// four is still refused, with a reason of its own, until
/// iamacoffeepot/aether#7451 binds one at dispatch: no registered
/// program holds a depth input, so the dispatch path never meets one.
fn depth_input_refusal(
    context: &PassValidation<'_>,
    index: usize,
    input_index: usize,
    slot: u32,
    attached_here: Option<u32>,
) -> String {
    let reads = format!("pass {index} input {input_index} reads depth transient {slot}");
    let declared = &context.mail.depth_transients;
    let Some(spec) = declared.get(slot as usize) else {
        return format!("{reads}, which is out of range ({} declared)", declared.len());
    };
    if spec.samples == Samples::Four {
        return format!(
            "{reads}, which is declared Four — a multisampled depth slot can be neither compared nor sampled, so a \
             pass reads a One slot",
        );
    }
    if attached_here == Some(slot) {
        return format!("{reads}, which the same pass attaches — a pass cannot read the depth slot it draws into");
    }
    let attached_earlier = context.earlier.iter().any(|earlier| earlier.stage.depth_slot() == Some(slot));
    if !attached_earlier {
        return format!("{reads} before any earlier pass attaches it");
    }
    format!("{reads}, and reading a depth slot from a pass is not bound yet (iamacoffeepot/aether#7451)")
}

fn check_binding_index(mail: &ProgramRegister, pass: usize, binding: u32) -> Result<(), String> {
    if (binding as usize) < mail.bindings.len() {
        Ok(())
    } else {
        Err(format!("pass {pass}: binding slot {binding} is out of range ({} declared)", mail.bindings.len()))
    }
}

fn check_transient_index(mail: &ProgramRegister, pass: usize, transient: u32) -> Result<(), String> {
    if (transient as usize) < mail.transients.len() {
        Ok(())
    } else {
        Err(format!("pass {pass}: transient slot {transient} is out of range ({} declared)", mail.transients.len()))
    }
}

/// Size in bytes of the uniform block the entry point actually uses at
/// `@group(0) @binding(0)`, from naga's layout info. `None` when the
/// entry point touches no such block — a uniform-less pass is fine with
/// any window, including a zero-length one.
fn uniform_block_bytes(module: &Module, info: &ModuleInfo, entry_index: usize) -> Option<u32> {
    let entry_info = info.get_entry_point(entry_index);
    module
        .global_variables
        .iter()
        .find(|(handle, var)| {
            matches!(var.space, AddressSpace::Uniform)
                && var.binding.as_ref().is_some_and(|binding| binding.group == 0 && binding.binding == 0)
                && !entry_info[*handle].is_empty()
        })
        .map(|(_, var)| module.types[var.ty].inner.size(module.to_ctx()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DepthRead, PassRepeat};

    const MODULE: &str = r"
struct WindowParams { value: f32 }
@group(0) @binding(0) var<uniform> window_params: WindowParams;
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@fragment
fn fs_copy(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, uv) * window_params.value;
}
";

    fn pass(entry: &str, inputs: Vec<InputSlot>, output: OutputSlot, offset: u32, length: u32) -> ProgramPass {
        ProgramPass {
            stage: PassStage::Fragment,
            blend: Blend::Alpha,
            entry_point: entry.to_owned(),
            inputs,
            output,
            uniform_offset: offset,
            uniform_length: length,
            repeat: None,
        }
    }

    fn full(format: TextureFormat) -> SlotSpec {
        SlotSpec {
            format,
            shape: SlotShape::Target(SlotExtent::Full),
            sampling: Sampling::Filtered { wrap: Wrap::Clamp, mips: Mips::Base },
        }
    }

    /// A full-extent transient of `samples` samples per texel.
    fn transient(format: TextureFormat, samples: Samples) -> TransientSpec {
        TransientSpec { format, extent: SlotExtent::Full, samples }
    }

    /// A full-extent depth slot of `samples` samples per texel.
    fn depth_slot(samples: Samples) -> DepthSpec {
        DepthSpec { extent: DepthExtent::Output(SlotExtent::Full), samples }
    }

    /// A ping-pong chain writes each hop to a fresh transient; the plan's
    /// live ranges must expire each transient the pass after its last
    /// read, or the dispatch-time pool assignment would either alias a
    /// live transient (corrupting the chain) or never reuse one (the
    /// three-hundred-allocation failure the ADR pools against).
    #[test]
    fn ping_pong_live_ranges_expire_after_last_read() {
        let mail = ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8)],
            transients: vec![transient(TextureFormat::Rgba8, Samples::One); 3],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![
                pass("fs_copy", vec![InputSlot::Binding { index: 0 }], OutputSlot::Transient { index: 0 }, 0, 4),
                pass("fs_copy", vec![InputSlot::Transient { index: 0 }], OutputSlot::Transient { index: 1 }, 0, 4),
                pass("fs_copy", vec![InputSlot::PassOutput { pass: 1 }], OutputSlot::Transient { index: 2 }, 0, 4),
                pass("fs_copy", vec![InputSlot::Transient { index: 2 }], OutputSlot::Binding { index: 1 }, 0, 4),
            ],
        };
        let plan = validate(&mail).expect("ping-pong chain validates");
        let ranges: Vec<(Option<u32>, Option<u32>)> =
            plan.transients.iter().map(|t| (t.first_write, t.last_use)).collect();
        // Transient 1's read through the PassOutput alias must extend its
        // range exactly as a direct Transient read would.
        assert_eq!(ranges, vec![(Some(0), Some(1)), (Some(1), Some(2)), (Some(2), Some(3))]);
        assert_eq!(plan.output_binding, 1);
        assert_eq!(plan.written_bindings, vec![1]);
    }

    /// Each register-time failure class replies its own distinguishable
    /// reason — collapsing them into one opaque string is the bug this
    /// pins, since callers triage a rejected program by its class.
    /// The rejection reason for a register mail that must not validate.
    fn rejection(mail: &ProgramRegister) -> String {
        match validate(mail) {
            Err(reason) => reason,
            Ok(_) => panic!("register must reject"),
        }
    }

    #[test]
    fn validation_classes_have_distinguishable_reasons() {
        let valid_pass =
            || pass("fs_copy", vec![InputSlot::Binding { index: 0 }], OutputSlot::Binding { index: 1 }, 0, 4);
        let base = || ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8)],
            transients: vec![],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![valid_pass()],
        };

        let bad_wgsl = rejection(&ProgramRegister { wgsl: "not wgsl at all".to_owned(), ..base() });
        assert!(bad_wgsl.starts_with("invalid wgsl:"), "naga class: {bad_wgsl}");

        let missing_entry = rejection(&ProgramRegister {
            passes: vec![ProgramPass { entry_point: "fs_missing".to_owned(), ..valid_pass() }],
            ..base()
        });
        assert!(missing_entry.contains("no fragment entry point"), "entry class: {missing_entry}");

        let unwritten_read = rejection(&ProgramRegister {
            transients: vec![transient(TextureFormat::Rgba8, Samples::One)],
            passes: vec![ProgramPass { inputs: vec![InputSlot::Transient { index: 0 }], ..valid_pass() }],
            ..base()
        });
        assert!(unwritten_read.contains("before any earlier pass writes it"), "sequence class: {unwritten_read}");

        let short_window =
            rejection(&ProgramRegister { passes: vec![ProgramPass { uniform_length: 2, ..valid_pass() }], ..base() });
        assert!(short_window.contains("uniform window"), "window class: {short_window}");

        let self_read = rejection(&ProgramRegister {
            passes: vec![ProgramPass { inputs: vec![InputSlot::Binding { index: 1 }], ..valid_pass() }],
            ..base()
        });
        assert!(self_read.contains("its own output"), "self-read class: {self_read}");

        let transient_tail = rejection(&ProgramRegister {
            transients: vec![transient(TextureFormat::Rgba8, Samples::One)],
            passes: vec![ProgramPass { output: OutputSlot::Transient { index: 0 }, ..valid_pass() }],
            ..base()
        });
        assert!(transient_tail.contains("final pass"), "final-output class: {transient_tail}");

        let zero_repeat = rejection(&ProgramRegister {
            passes: vec![ProgramPass { repeat: Some(PassRepeat { count: 0, uniform_stride: 0 }), ..valid_pass() }],
            ..base()
        });
        assert!(zero_repeat.contains("repeat count"), "repeat class: {zero_repeat}");

        let zero_divisor = rejection(&ProgramRegister {
            transients: vec![TransientSpec {
                extent: SlotExtent::Divided { divisor: 0 },
                ..transient(TextureFormat::Rgba8, Samples::One)
            }],
            ..base()
        });
        assert!(zero_divisor.contains("divisor must be at least 1"), "divisor class: {zero_divisor}");

        let binding_past_list = rejection(&ProgramRegister {
            passes: vec![ProgramPass { inputs: vec![InputSlot::Binding { index: 7 }], ..valid_pass() }],
            ..base()
        });
        assert!(binding_past_list.contains("binding slot 7 is out of range"), "binding class: {binding_past_list}");
    }

    /// The executor's per-dispatch cost is declared by the graph, so both
    /// ceilings must reject at register — the bugs pinned are a graph of
    /// many maximally-repeated passes asking the executor to encode
    /// millions of render passes (a stalled frame), and the same shape
    /// staging gigabytes of uniform windows from a small mail, which
    /// overflows the `u32` offset `encode_passes` casts and panics the
    /// driver thread. `MAX_REPEAT_COUNT` bounds one entry and neither of
    /// these, which is why they are checked over the whole pass list.
    #[test]
    fn encode_budget_ceilings_reject_at_register() {
        let base = |passes: Vec<ProgramPass>| ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8)],
            transients: vec![],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes,
        };
        let repeated = |length: u32| ProgramPass {
            repeat: Some(PassRepeat { count: MAX_REPEAT_COUNT, uniform_stride: 0 }),
            ..pass("fs_copy", vec![InputSlot::Binding { index: 0 }], OutputSlot::Binding { index: 1 }, 0, length)
        };

        // 32 x 4096 = 131072 iterations, past the iteration ceiling while
        // each window stays small.
        let too_many_iterations = rejection(&base((0..32).map(|_| repeated(4)).collect()));
        assert!(too_many_iterations.contains("pass iterations per dispatch"), "iteration class: {too_many_iterations}");

        // 8 x 4096 = 32768 iterations (under the iteration ceiling) but
        // 8 x 4096 x 4 KiB = 128 MiB staged, past the byte ceiling.
        let too_many_bytes = rejection(&base((0..8).map(|_| repeated(4096)).collect()));
        assert!(too_many_bytes.contains("uniform bytes per dispatch"), "staging class: {too_many_bytes}");

        // The named consumer's shape — one maximally-repeated pass over a
        // small window — stays comfortably inside both ceilings.
        assert!(validate(&base(vec![repeated(64)])).is_ok(), "a wash-shaped chain must still register");
    }

    /// A read-only slot of the given shape, read texel by texel.
    fn read_only(shape: SlotShape) -> SlotSpec {
        SlotSpec { format: TextureFormat::Rgba8, shape, sampling: Sampling::Texel }
    }

    /// The two-binding copy program the shape rules are checked on:
    /// binding 0 read, binding 1 written by the only pass.
    fn copy_program(bindings: Vec<SlotSpec>) -> ProgramRegister {
        ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings,
            transients: Vec::new(),
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![pass(
                "fs_copy",
                vec![InputSlot::Binding { index: 0 }],
                OutputSlot::Binding { index: 1 },
                0,
                4,
            )],
        }
    }

    /// A read-only texture attached as a render target: the dispatch
    /// check asks only that a written binding's texture is writable, so
    /// a writable texture of any size bound at a `Texture` binding would
    /// be rendered into at a size the graph never agreed to. The pass
    /// before the final one is the writer here, so the refusal is the
    /// pass rule and not the final-output rule.
    #[test]
    fn a_pass_writing_a_texture_binding_is_refused() {
        let mut mail =
            copy_program(vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8), read_only(SlotShape::Texture)]);
        mail.passes
            .insert(0, pass("fs_copy", vec![InputSlot::Binding { index: 0 }], OutputSlot::Binding { index: 2 }, 0, 4));

        let reason = rejection(&mail);
        assert!(reason.contains("pass 0: binding 2"), "the refusal names pass and binding: {reason}");
        assert!(reason.contains("read only"), "read-only class: {reason}");
    }

    /// The final output's size is the reference every extent scales
    /// from. A final output with no extent leaves the program without
    /// one, and the transient pool would size from nothing.
    #[test]
    fn a_final_output_that_is_not_a_full_target_is_refused() {
        let halved =
            SlotSpec { shape: SlotShape::Target(SlotExtent::Divided { divisor: 2 }), ..full(TextureFormat::Rgba8) };
        let reason = rejection(&copy_program(vec![full(TextureFormat::Rgba8), halved]));
        assert!(reason.contains("must declare Target(Full)"), "final-output class: {reason}");

        let any_size = copy_program(vec![full(TextureFormat::Rgba8), read_only(SlotShape::Texture)]);
        let reason = rejection(&any_size);
        assert!(reason.contains("binding 1"), "the refusal names the binding: {reason}");
    }

    /// A `Texture` binding has no extent, so a register that still
    /// demanded one of every binding would refuse the shape outright.
    /// The plan keeps the declaration, which is what the layout and the
    /// dispatch check read.
    #[test]
    fn a_texel_texture_binding_validates() {
        let table = read_only(SlotShape::Texture);
        let plan = validate(&copy_program(vec![table, full(TextureFormat::Rgba8)]))
            .expect("a read-only binding of its own size validates");
        assert_eq!(plan.slot_spec(ResolvedSlot::Binding(0)), table);
        assert_eq!(plan.output_binding, 1);
    }

    /// A draw-pass module: one entry per shape the draw validation has
    /// to see. `vs_flat` reads position alone and takes its clip depth
    /// from the uniform window (the vertex stage reading group 0 is what
    /// makes the window's visibility load-bearing); `vs_tinted` reads a
    /// second location; `fs_depth_writer` writes `@builtin(frag_depth)`
    /// beside a color. `fs_nothing` and `fs_depth_alone` return no
    /// color, which is what a depth-only pass's entry point does.
    const DRAW_MODULE: &str = r"
struct DrawParams { color: vec4<f32>, depth: f32 }
@group(0) @binding(0) var<uniform> draw_params: DrawParams;

@vertex
fn vs_flat(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position.xy, draw_params.depth, 1.0);
}

@vertex
fn vs_tinted(@location(0) position: vec3<f32>, @location(1) tint: vec4<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position.xy, tint.x, 1.0);
}

@fragment
fn fs_flat() -> @location(0) vec4<f32> {
    return draw_params.color;
}

@fragment
fn fs_opaque() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}

struct DepthOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}

@fragment
fn fs_depth_writer() -> DepthOut {
    return DepthOut(draw_params.color, draw_params.depth);
}

@fragment
fn fs_nothing() {}

@fragment
fn fs_depth_alone() -> @builtin(frag_depth) f32 {
    return draw_params.depth;
}
";

    /// Bytes of `DrawParams`: a `vec4<f32>` then an `f32`, padded out to
    /// the struct's 16-byte alignment.
    const DRAW_PARAMS_BYTES: u32 = 32;

    fn position_slot() -> GeometrySlotSpec {
        GeometrySlotSpec { layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }] }
    }

    fn draw_stage(vertex_entry: &str, geometry: u32, depth: Option<u32>) -> PassStage {
        PassStage::Draw(DrawPass {
            vertex_entry_point: vertex_entry.to_owned(),
            geometry,
            depth,
            load: PassLoad::Clear,
        })
    }

    /// A draw of geometry slot 0 into binding 0 under depth slot 0.
    fn draw_pass() -> ProgramPass {
        ProgramPass {
            stage: draw_stage("vs_flat", 0, Some(0)),
            blend: Blend::Alpha,
            entry_point: "fs_flat".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: DRAW_PARAMS_BYTES,
            repeat: None,
        }
    }

    /// ADR-0171 draw validation: each new failure class replies its own
    /// distinguishable reason. The bugs pinned, one per class: a typo'd
    /// vertex entry or geometry slot reaching wgpu as an opaque
    /// `pipeline creation failed`; a vertex stage reading an attribute
    /// the bound geometry never supplies (undefined vertex data rather
    /// than a rejected register); a location whose WGSL type disagrees
    /// with the declared format, which reads the same bytes as a
    /// different quantity; a depth slot that cannot attach to its color
    /// output because their extents differ; and a fragment stage
    /// writing `@builtin(frag_depth)` into a pass with no depth
    /// attachment to receive it.
    #[test]
    fn draw_validation_classes_have_distinguishable_reasons() {
        let base = || ProgramRegister {
            wgsl: DRAW_MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8)],
            transients: Vec::new(),
            geometries: vec![position_slot()],
            depth_transients: vec![depth_slot(Samples::One)],
            passes: vec![draw_pass()],
        };

        let plan = validate(&base()).expect("the baseline draw program validates");
        let drawn = plan.passes[0].stage.draw().expect("the draw pass carries a draw plan");
        assert_eq!(drawn.vertex_entry_point, "vs_flat");
        assert_eq!(drawn.depth, Some(0));

        let missing_vertex = rejection(&ProgramRegister {
            passes: vec![ProgramPass { stage: draw_stage("vs_missing", 0, Some(0)), ..draw_pass() }],
            ..base()
        });
        assert!(missing_vertex.contains("no vertex entry point"), "vertex-entry class: {missing_vertex}");

        let bad_slot = rejection(&ProgramRegister {
            passes: vec![ProgramPass { stage: draw_stage("vs_flat", 3, Some(0)), ..draw_pass() }],
            ..base()
        });
        assert!(bad_slot.contains("geometry slot 3 is out of range"), "geometry-range class: {bad_slot}");

        let undeclared_location = rejection(&ProgramRegister {
            passes: vec![ProgramPass { stage: draw_stage("vs_tinted", 0, Some(0)), ..draw_pass() }],
            ..base()
        });
        assert!(
            undeclared_location.contains("@location(1)") && undeclared_location.contains("does not declare"),
            "unbound-location class: {undeclared_location}",
        );

        let wrong_format = rejection(&ProgramRegister {
            geometries: vec![GeometrySlotSpec {
                layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x2 }],
            }],
            ..base()
        });
        assert!(wrong_format.contains("consumed as vec2<f32>"), "format-mismatch class: {wrong_format}");

        let duplicate_location = rejection(&ProgramRegister {
            geometries: vec![GeometrySlotSpec {
                layout: vec![
                    VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
                    VertexAttribute { location: 0, format: VertexFormat::Float32 },
                ],
            }],
            ..base()
        });
        assert!(duplicate_location.contains("location 0 twice"), "duplicate-location class: {duplicate_location}");

        let bad_depth = rejection(&ProgramRegister {
            passes: vec![ProgramPass { stage: draw_stage("vs_flat", 0, Some(4)), ..draw_pass() }],
            ..base()
        });
        assert!(bad_depth.contains("depth transient 4 is out of range"), "depth-range class: {bad_depth}");

        let halved =
            DepthSpec { extent: DepthExtent::Output(SlotExtent::Divided { divisor: 2 }), ..depth_slot(Samples::One) };
        let mismatched_depth = rejection(&ProgramRegister { depth_transients: vec![halved], ..base() });
        assert!(
            mismatched_depth.contains("does not match its color output's extent"),
            "depth-extent class: {mismatched_depth}",
        );

        let undeclared_depth = rejection(&ProgramRegister {
            passes: vec![ProgramPass {
                stage: draw_stage("vs_flat", 0, None),
                entry_point: "fs_depth_writer".to_owned(),
                ..draw_pass()
            }],
            ..base()
        });
        assert!(undeclared_depth.contains("must declare a depth transient"), "frag-depth class: {undeclared_depth}");
    }

    /// The uniform window must cover the block whichever stage reads it:
    /// a draw pass whose *vertex* stage is the only reader of group 0
    /// still needs a window long enough, or the pipeline binds a buffer
    /// shorter than the shader's declared block and wgpu rejects it at
    /// register — an opaque failure instead of the named window class.
    #[test]
    fn draw_uniform_window_covers_the_vertex_stage_block() {
        let mail = ProgramRegister {
            wgsl: DRAW_MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8)],
            transients: Vec::new(),
            geometries: vec![position_slot()],
            depth_transients: Vec::new(),
            passes: vec![ProgramPass {
                stage: draw_stage("vs_flat", 0, None),
                // A fragment entry that reads nothing from group 0, so
                // only the vertex stage's use can drive the check.
                entry_point: "fs_opaque".to_owned(),
                uniform_length: 4,
                ..draw_pass()
            }],
        };
        let short = rejection(&mail);
        assert!(short.contains("uniform window"), "window class over the vertex stage: {short}");
    }

    /// A draw into transient 0 under depth slot 0, then a draw into the
    /// output binding with no depth, with the transient and the depth
    /// slot declared as given.
    fn multisampled_draw(color: Samples, depth: Samples) -> ProgramRegister {
        ProgramRegister {
            wgsl: DRAW_MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8)],
            transients: vec![transient(TextureFormat::Rgba8, color)],
            geometries: vec![position_slot()],
            depth_transients: vec![depth_slot(depth)],
            passes: vec![
                ProgramPass { output: OutputSlot::Transient { index: 0 }, ..draw_pass() },
                ProgramPass { stage: draw_stage("vs_flat", 0, None), ..draw_pass() },
            ],
        }
    }

    /// The bug: a `One` depth slot under a `Four` color output
    /// registers, and wgpu then refuses every dispatch at
    /// `begin_render_pass` because the attachments of one pass disagree
    /// on their sample count.
    #[test]
    fn a_depth_slot_of_another_sample_count_is_refused() {
        validate(&multisampled_draw(Samples::Four, Samples::Four)).expect("a Four depth slot under a Four output");

        let reason = rejection(&multisampled_draw(Samples::Four, Samples::One));
        assert!(reason.contains("pass 0: depth transient 0"), "the refusal names pass and depth slot: {reason}");
        assert!(reason.contains("samples One") && reason.contains("samples Four"), "and both counts: {reason}");
    }

    /// The bug: a `Four` `R32Float` transient registers, and the resolve
    /// a reader needs is refused by wgpu on every dispatch — core
    /// WebGPU multisamples that format and cannot resolve it.
    #[test]
    fn a_four_transient_that_cannot_resolve_is_refused() {
        let declares = |format: TextureFormat| ProgramRegister {
            transients: vec![transient(format, Samples::Four)],
            ..copy_program(vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8)])
        };
        validate(&declares(TextureFormat::Rgba16Float)).expect("a Four transient of a format that resolves");

        let reason = rejection(&declares(TextureFormat::R32Float));
        assert!(reason.contains("transient 0"), "the refusal names the transient: {reason}");
        assert!(reason.contains("R32Float cannot be resolved"), "and the format: {reason}");
    }

    /// A depth-only draw of geometry slot 0 into depth slot 0 through
    /// the fragment entry point `entry`.
    fn depth_only_pass(entry: &str) -> ProgramPass {
        ProgramPass {
            stage: PassStage::Draw(DrawPass {
                vertex_entry_point: "vs_flat".to_owned(),
                geometry: 0,
                depth: Some(0),
                load: PassLoad::Load,
            }),
            blend: Blend::Replace,
            entry_point: entry.to_owned(),
            output: OutputSlot::None,
            ..draw_pass()
        }
    }

    /// A draw into binding 0 with no depth: the final pass a program
    /// whose other passes are depth-only still needs.
    fn color_pass() -> ProgramPass {
        ProgramPass { stage: draw_stage("vs_flat", 0, None), ..draw_pass() }
    }

    /// `passes` over one binding, one geometry slot and `depth_transients`.
    fn depth_program(depth_transients: Vec<DepthSpec>, passes: Vec<ProgramPass>) -> ProgramRegister {
        ProgramRegister {
            wgsl: DRAW_MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8)],
            transients: Vec::new(),
            geometries: vec![position_slot()],
            depth_transients,
            passes,
        }
    }

    /// The bugs: a rasterizing pass with no color output still refused
    /// as "must declare a texture output", so no shadow map registers;
    /// and its pipeline built at one sample because it has no output to
    /// ask, which wgpu refuses against a `Four` depth attachment on
    /// every dispatch. A `Fixed` slot stands in for the shadow map: a
    /// depth-only pass attaches either extent.
    #[test]
    fn depth_only_passes_validate_and_rasterize_at_their_depth_slots_samples() {
        let shadow_map = DepthSpec { extent: DepthExtent::Fixed { side: 512 }, samples: Samples::Four };
        let passes = vec![depth_only_pass("fs_nothing"), depth_only_pass("fs_depth_alone"), color_pass()];

        let plan = validate(&depth_program(vec![shadow_map], passes)).expect("depth-only passes validate");

        assert_eq!(plan.passes[0].output, None);
        assert_eq!(plan.passes[1].output, None);
        assert_eq!(plan.pass_samples(&plan.passes[0]), Samples::Four, "a depth-only pass takes its depth slot's count");
        assert_eq!(plan.pass_samples(&plan.passes[2]), Samples::One, "a pass with an output takes the output's");
        assert_eq!(plan.output_binding, 0);
    }

    /// Each way a rasterizing pass with no color output can be
    /// malformed has its own reason. The bugs, one per class: a pass
    /// that attaches nothing at all reaching wgpu; a blend or a color
    /// clear that registers and then silently does nothing; and an
    /// entry point returning a color into a pipeline with no color
    /// target, which comes back as an opaque `pipeline creation failed`
    /// (`fs_depth_writer` returns one beside `frag_depth`, so a check
    /// that only asked "does it write depth" would let it through).
    #[test]
    fn depth_only_refusal_classes_have_distinguishable_reasons() {
        let refused =
            |pass: ProgramPass| rejection(&depth_program(vec![depth_slot(Samples::One)], vec![pass, color_pass()]));

        let no_slot = refused(ProgramPass {
            stage: PassStage::Draw(DrawPass {
                vertex_entry_point: "vs_flat".to_owned(),
                geometry: 0,
                depth: None,
                load: PassLoad::Load,
            }),
            ..depth_only_pass("fs_nothing")
        });
        assert!(no_slot.contains("pass 0") && no_slot.contains("must name a depth transient"), "no-slot: {no_slot}");

        let blended = refused(ProgramPass { blend: Blend::Additive, ..depth_only_pass("fs_nothing") });
        assert!(blended.contains("Blend::Replace, not Additive"), "depth-only blend class: {blended}");

        let cleared =
            refused(ProgramPass { stage: draw_stage("vs_flat", 0, Some(0)), ..depth_only_pass("fs_nothing") });
        assert!(cleared.contains("PassLoad::Load, not Clear"), "depth-only load class: {cleared}");

        let colored = refused(depth_only_pass("fs_flat"));
        assert!(colored.contains("`fs_flat` returns a color"), "depth-only entry class: {colored}");

        let colored_beside_depth = refused(depth_only_pass("fs_depth_writer"));
        assert!(
            colored_beside_depth.contains("`fs_depth_writer` returns a color"),
            "a color beside frag_depth is still a color: {colored_beside_depth}",
        );
    }

    /// The bug: a `Fixed` slot registers beside a color output, and the
    /// first dispatch whose output is not that exact square fails at
    /// `begin_render_pass` and drops, with the reason only in a log.
    #[test]
    fn a_fixed_depth_slot_under_a_color_output_is_refused() {
        let fixed = DepthSpec { extent: DepthExtent::Fixed { side: 64 }, samples: Samples::One };

        let reason = rejection(&depth_program(vec![fixed], vec![draw_pass()]));

        assert!(reason.contains("pass 0: depth transient 0 declares a fixed side 64"), "names the slot: {reason}");
        assert!(reason.contains("not known at register"), "fixed-under-color class: {reason}");
    }

    /// The bug: a side of zero or past the device limit registers, and
    /// texture creation fails inside the first dispatch.
    #[test]
    fn a_fixed_side_outside_the_texture_limit_is_refused() {
        let limit = render_limits().max_texture_dimension_2d;
        let sided = |side: u32| {
            let slot = DepthSpec { extent: DepthExtent::Fixed { side }, samples: Samples::One };
            depth_program(vec![slot], vec![depth_only_pass("fs_nothing"), color_pass()])
        };
        validate(&sided(limit)).expect("a side at the limit validates");

        let zero = rejection(&sided(0));
        assert!(zero.contains("depth transient 0: fixed side 0 is outside"), "zero-side class: {zero}");

        let oversized = rejection(&sided(limit + 1));
        assert!(oversized.contains(&format!("is outside 1..={limit}")), "names the limit: {oversized}");
    }

    /// The bug: `PassOutput` naming a depth-only pass resolves to
    /// nothing and the reason blames a compute pass the graph does not
    /// have.
    #[test]
    fn pass_output_cannot_name_a_depth_only_pass() {
        let reader = ProgramPass { inputs: vec![InputSlot::PassOutput { pass: 0 }], ..color_pass() };

        let reason =
            rejection(&depth_program(vec![depth_slot(Samples::One)], vec![depth_only_pass("fs_nothing"), reader]));

        assert!(reason.contains("pass 1 reads pass 0 through PassOutput"), "names both passes: {reason}");
        assert!(reason.contains("a depth-only pass"), "outputless alias class: {reason}");
    }

    /// The refusals of a depth input, in the order decision 9 lists
    /// them, then the one that stands until the read is bound. The
    /// bugs, one per class: an index past the list panicking the
    /// lookup; a `Four` slot registering though nothing can sample it;
    /// a pass reading the slot it attaches, which the device refuses on
    /// every dispatch; a read of a slot nothing has drawn; and a
    /// well-formed depth input registering `Ok` and then meeting a
    /// dispatch path that cannot bind it.
    #[test]
    fn depth_input_refusals_have_distinguishable_reasons() {
        let reads = |index: u32| vec![InputSlot::Depth { index, read: DepthRead::Compare }];
        let slots = || vec![depth_slot(Samples::One), depth_slot(Samples::Four)];
        let refused = |passes: Vec<ProgramPass>| rejection(&depth_program(slots(), passes));

        let past_list = refused(vec![ProgramPass { inputs: reads(7), ..color_pass() }]);
        assert!(past_list.contains("reads depth transient 7, which is out of range"), "range class: {past_list}");

        let multisampled = refused(vec![ProgramPass { inputs: reads(1), ..color_pass() }]);
        assert!(multisampled.contains("which is declared Four"), "four-slot class: {multisampled}");

        let self_read = refused(vec![ProgramPass { inputs: reads(0), ..draw_pass() }]);
        assert!(self_read.contains("which the same pass attaches"), "same-pass class: {self_read}");

        let unwritten = refused(vec![ProgramPass { inputs: reads(0), ..color_pass() }]);
        assert!(unwritten.contains("before any earlier pass attaches it"), "sequence class: {unwritten}");

        let well_formed =
            refused(vec![depth_only_pass("fs_nothing"), ProgramPass { inputs: reads(0), ..color_pass() }]);
        assert!(well_formed.contains("pass 1 input 0 reads depth transient 0"), "names the input: {well_formed}");
        assert!(well_formed.contains("not bound yet (iamacoffeepot/aether#7451)"), "unbound class: {well_formed}");
    }

    const COMPUTE_MODULE: &str = r"
@group(2) @binding(0) var<storage, read> source_vertices: array<u32>;
@group(2) @binding(1) var<storage, read_write> output_vertices: array<u32>;
@group(2) @binding(2) var<storage, read_write> output_indices: array<u32>;
@group(2) @binding(3) var<storage, read_write> indirect: array<u32>;

@compute @workgroup_size(1)
fn cs_derive() {
    output_vertices[0] = source_vertices[0];
    output_indices[0] = 0u;
    indirect[0] = 3u;
}

@vertex
fn vs_flat(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position, 1.0);
}

@fragment
fn fs_red() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}
";

    fn compute_stage() -> PassStage {
        PassStage::Compute(ComputePass {
            buffers: vec![
                ComputeBufferBinding { geometry: 0, buffer: GeometryBuffer::Vertices, access: StorageAccess::Read },
                ComputeBufferBinding {
                    geometry: 1,
                    buffer: GeometryBuffer::Vertices,
                    access: StorageAccess::ReadWrite,
                },
                ComputeBufferBinding { geometry: 1, buffer: GeometryBuffer::Indices, access: StorageAccess::ReadWrite },
                ComputeBufferBinding {
                    geometry: 1,
                    buffer: GeometryBuffer::DrawIndexedIndirect,
                    access: StorageAccess::ReadWrite,
                },
            ],
            workgroups: [1, 1, 1],
        })
    }

    fn compute_pass() -> ProgramPass {
        ProgramPass {
            stage: compute_stage(),
            blend: Blend::Replace,
            entry_point: "cs_derive".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::None,
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }
    }

    fn indirect_pass() -> ProgramPass {
        ProgramPass {
            stage: PassStage::DrawIndexedIndirect(DrawPass {
                vertex_entry_point: "vs_flat".to_owned(),
                geometry: 1,
                depth: None,
                load: PassLoad::Clear,
            }),
            blend: Blend::Alpha,
            entry_point: "fs_red".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }
    }

    fn compute_program() -> ProgramRegister {
        ProgramRegister {
            wgsl: COMPUTE_MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8)],
            transients: Vec::new(),
            geometries: vec![position_slot(), position_slot()],
            depth_transients: Vec::new(),
            passes: vec![compute_pass(), indirect_pass()],
        }
    }

    #[test]
    fn compute_and_indirect_plan_validates_exact_storage_contract() {
        let plan = validate(&compute_program()).expect("compute-to-indirect graph validates");
        let compute = plan.passes[0].stage.compute().expect("first pass is compute");
        assert_eq!(compute.workgroups, [1, 1, 1]);
        assert_eq!(compute.buffers.len(), 4);
        assert!(matches!(plan.passes[1].stage, PassPlanStage::DrawIndexedIndirect(_)));
        assert_eq!(plan.passes[0].output, None);
        assert_eq!(plan.output_binding, 0);
    }

    #[test]
    fn compute_validation_classes_are_distinguishable() {
        let mut nonempty_output = compute_program();
        nonempty_output.passes[0].output = OutputSlot::Binding { index: 0 };
        let reason = rejection(&nonempty_output);
        assert!(reason.contains("OutputSlot::None"), "compute-output class: {reason}");

        let mut missing_entry = compute_program();
        missing_entry.passes[0].entry_point = "cs_missing".to_owned();
        let reason = rejection(&missing_entry);
        assert!(reason.contains("no compute entry point"), "compute-entry class: {reason}");

        let mut zero_workgroups = compute_program();
        let PassStage::Compute(compute) = &mut zero_workgroups.passes[0].stage else {
            panic!("fixture starts compute");
        };
        compute.workgroups[1] = 0;
        let reason = rejection(&zero_workgroups);
        assert!(reason.contains("dimension 1 must be at least 1"), "workgroup class: {reason}");

        let mut bad_geometry = compute_program();
        let PassStage::Compute(compute) = &mut bad_geometry.passes[0].stage else {
            panic!("fixture starts compute");
        };
        compute.buffers[0].geometry = 8;
        let reason = rejection(&bad_geometry);
        assert!(reason.contains("geometry slot 8") && reason.contains("out of range"), "geometry class: {reason}");

        let mut duplicate = compute_program();
        let PassStage::Compute(compute) = &mut duplicate.passes[0].stage else {
            panic!("fixture starts compute");
        };
        compute.buffers[1] = compute.buffers[0];
        let reason = rejection(&duplicate);
        assert!(reason.contains("more than once"), "storage-alias class: {reason}");

        let mut wrong_access = compute_program();
        let PassStage::Compute(compute) = &mut wrong_access.passes[0].stage else {
            panic!("fixture starts compute");
        };
        compute.buffers[1].access = StorageAccess::Read;
        let reason = rejection(&wrong_access);
        assert!(reason.contains("access disagrees"), "storage-access class: {reason}");

        let mut no_writer = compute_program();
        no_writer.passes.remove(0);
        let reason = rejection(&no_writer);
        assert!(reason.contains("no preceding compute pass"), "indirect-dependency class: {reason}");
    }

    /// The bug: a compute pass declaring `Additive` registers, and the
    /// blend it declared silently does nothing — a compute pass has no
    /// color output for it to apply to.
    #[test]
    fn a_compute_pass_declaring_a_blend_is_refused() {
        let mut blended = compute_program();
        blended.passes[0].blend = Blend::Additive;

        let reason = rejection(&blended);
        assert!(reason.contains("pass 0: a compute pass"), "the refusal names the pass: {reason}");
        assert!(reason.contains("Blend::Replace, not Additive"), "compute-blend class: {reason}");
    }

    #[test]
    fn pass_output_cannot_name_outputless_compute() {
        let mut mail = compute_program();
        mail.passes.insert(
            1,
            ProgramPass {
                stage: PassStage::Fragment,
                blend: Blend::Alpha,
                entry_point: "fs_red".to_owned(),
                inputs: vec![InputSlot::PassOutput { pass: 0 }],
                output: OutputSlot::Binding { index: 0 },
                uniform_offset: 0,
                uniform_length: 0,
                repeat: None,
            },
        );
        let reason = rejection(&mail);
        assert!(reason.contains("that pass has no texture output"), "outputless alias class: {reason}");
    }
}
