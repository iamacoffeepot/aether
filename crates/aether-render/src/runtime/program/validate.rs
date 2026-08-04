//! Register-time program validation (ADR-0170): the WGSL through naga,
//! then the declared graph — pure CPU, no device. Success produces the
//! [`ProgramPlan`] the register path builds pipelines from and the
//! dispatch path records against; failure produces the `Err { reason }`
//! string, one distinguishable message per validation class.

use naga::front::wgsl;
use naga::valid::{Capabilities, ModuleInfo, ValidationFlags, Validator};
use naga::{
    AddressSpace, Binding, BuiltIn, Handle, Module, Scalar, ScalarKind, ShaderStage, Type, TypeInner, VectorSize,
};

use crate::{
    DrawPass, GeometrySlotSpec, InputSlot, OutputSlot, PassLoad, PassStage, ProgramPass, ProgramRegister, SlotExtent,
    SlotSpec, TextureFormat, VertexAttribute, VertexFormat,
};

/// Ceiling on one pass's repeat count: a register-time bound so a typo
/// cannot ask the executor to encode an effectively unbounded number of
/// render passes per dispatch. Generous against the named consumer (a
/// wash chain is hundreds of pours).
const MAX_REPEAT_COUNT: u32 = 4096;

/// A pass slot with the register-only `PassOutput` alias resolved away:
/// what the executor actually binds or attaches.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ResolvedSlot {
    /// The dispatch binding at this index.
    Binding(u32),
    /// The transient intermediate at this index.
    Transient(u32),
}

/// One validated pass: entry point, resolved slots, uniform window, and
/// the flattened repeat (`repeat_count` is 1 for an unrepeated pass).
/// `draw` is `Some` for a `PassStage::Draw` pass and `None` for a
/// fullscreen fragment pass.
#[derive(Debug)]
pub struct PassPlan {
    pub entry_point: String,
    pub draw: Option<DrawPlan>,
    pub inputs: Vec<ResolvedSlot>,
    pub output: ResolvedSlot,
    pub uniform_offset: u32,
    pub uniform_length: u32,
    pub repeat_count: u32,
    pub uniform_stride: u32,
}

/// One validated draw pass (ADR-0171): the authored vertex entry, the
/// geometry slot the dispatch fills, the depth slot it clears and tests
/// against (`None` for a pass that does not depth-test), and the color
/// load semantic it declared.
#[derive(Debug)]
pub struct DrawPlan {
    pub vertex_entry_point: String,
    pub geometry: u32,
    pub depth: Option<u32>,
    pub load: PassLoad,
}

/// One transient's declaration plus its live range over the pass
/// sequence — `first_write..=last_use`, `None` when no pass references
/// it. The dispatch-time pool assignment reuses a physical texture
/// whose previous holder's `last_use` lies strictly before the next
/// holder's `first_write`.
#[derive(Debug)]
pub struct TransientPlan {
    pub spec: SlotSpec,
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
    /// Declared depth transients (ADR-0171), by extent — the format is
    /// fixed at `Depth32Float`.
    pub depth_transients: Vec<SlotExtent>,
    pub passes: Vec<PassPlan>,
    /// The dispatch binding the final pass writes — the program's
    /// result texture, whose size is the reference extent.
    pub output_binding: u32,
    /// Deduplicated binding indices any pass writes; each must resolve
    /// to a `Writable` registry texture at dispatch.
    pub written_bindings: Vec<u32>,
}

impl ProgramPlan {
    /// The declared format of a resolved slot.
    pub fn slot_format(&self, slot: ResolvedSlot) -> TextureFormat {
        self.slot_spec(slot).format
    }

    /// The declared spec of a resolved slot.
    pub fn slot_spec(&self, slot: ResolvedSlot) -> SlotSpec {
        match slot {
            ResolvedSlot::Binding(index) => self.bindings[index as usize],
            ResolvedSlot::Transient(index) => self.transients[index as usize].spec,
        }
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
        check_extent(spec.extent, || format!("binding {index}"))?;
    }
    for (index, spec) in mail.transients.iter().enumerate() {
        check_extent(spec.extent, || format!("transient {index}"))?;
    }
    for (index, extent) in mail.depth_transients.iter().enumerate() {
        check_extent(*extent, || format!("depth transient {index}"))?;
    }
    for (index, slot) in mail.geometries.iter().enumerate() {
        check_geometry_slot(index, slot)?;
    }

    let mut transients: Vec<TransientPlan> =
        mail.transients.iter().map(|spec| TransientPlan { spec: *spec, first_write: None, last_use: None }).collect();
    let mut passes: Vec<PassPlan> = Vec::with_capacity(mail.passes.len());
    let mut written_bindings: Vec<u32> = Vec::new();
    for (index, pass) in mail.passes.iter().enumerate() {
        let plan = validate_pass(mail, &module, &info, &passes, &transients, index, pass)?;
        let sequence = u32::try_from(index).expect("pass sequence index fits u32");
        if let ResolvedSlot::Transient(transient) = plan.output {
            let live = &mut transients[transient as usize];
            live.first_write.get_or_insert(sequence);
            live.last_use = Some(sequence);
        }
        for input in &plan.inputs {
            if let ResolvedSlot::Transient(transient) = input {
                transients[*transient as usize].last_use = Some(sequence);
            }
        }
        if let ResolvedSlot::Binding(binding) = plan.output
            && !written_bindings.contains(&binding)
        {
            written_bindings.push(binding);
        }
        passes.push(plan);
    }

    let final_output = passes.last().expect("passes checked non-empty").output;
    let ResolvedSlot::Binding(output_binding) = final_output else {
        return Err("the final pass must write a dispatch binding (the program's result texture)".to_owned());
    };
    if mail.bindings[output_binding as usize].extent != SlotExtent::Full {
        return Err(format!(
            "binding {output_binding}: the program's output binding must declare Full extent — its texture's size \
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
    })
}

fn check_extent(extent: SlotExtent, slot: impl Fn() -> String) -> Result<(), String> {
    match extent {
        SlotExtent::Divided { divisor: 0 } => Err(format!("{}: extent divisor must be at least 1", slot())),
        SlotExtent::Full | SlotExtent::Divided { .. } => Ok(()),
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

// One linear walk per pass — entry point, inputs, output, window,
// repeat — reads better in sequence than split into per-check helpers
// that would each re-thread the same five context arguments.
#[allow(clippy::too_many_arguments)]
fn validate_pass(
    mail: &ProgramRegister,
    module: &Module,
    info: &ModuleInfo,
    earlier: &[PassPlan],
    transients: &[TransientPlan],
    index: usize,
    pass: &ProgramPass,
) -> Result<PassPlan, String> {
    // Matched rather than destructured so a future `Compute` arm fails
    // to compile here rather than silently building a fragment pipeline.
    let declared_draw = match &pass.stage {
        PassStage::Fragment => None,
        PassStage::Draw(draw) => Some(draw),
    };

    let entry_index = module
        .entry_points
        .iter()
        .position(|entry| entry.stage == ShaderStage::Fragment && entry.name == pass.entry_point)
        .ok_or_else(|| format!("pass {index}: no fragment entry point named `{}` in the module", pass.entry_point))?;

    let mut inputs = Vec::with_capacity(pass.inputs.len());
    for (input_index, input) in pass.inputs.iter().enumerate() {
        inputs.push(resolve_input(mail, earlier, transients, index, input_index, *input)?);
    }
    let output = match pass.output {
        OutputSlot::Binding { index: binding } => {
            check_binding_index(mail, index, binding)?;
            ResolvedSlot::Binding(binding)
        }
        OutputSlot::Transient { index: transient } => {
            check_transient_index(mail, index, transient)?;
            ResolvedSlot::Transient(transient)
        }
    };
    if inputs.contains(&output) {
        return Err(format!("pass {index} reads its own output slot"));
    }

    let output_extent = match output {
        ResolvedSlot::Binding(binding) => mail.bindings[binding as usize].extent,
        ResolvedSlot::Transient(transient) => mail.transients[transient as usize].extent,
    };
    let draw = declared_draw
        .map(|declared| validate_draw(mail, module, index, declared, (entry_index, &pass.entry_point), output_extent))
        .transpose()?;

    // The window must cover whichever stages read the block: a draw
    // pass's vertex stage binds the same group-0 window its fragment
    // stage does.
    let block_bytes = [Some(entry_index), draw.as_ref().map(|resolved| resolved.vertex_entry_index)]
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
        draw: draw.map(|validated| validated.plan),
        inputs,
        output,
        uniform_offset: pass.uniform_offset,
        uniform_length: pass.uniform_length,
        repeat_count,
        uniform_stride,
    })
}

/// A validated draw declaration plus the naga entry index of its vertex
/// stage, which the caller needs for the shared uniform-window check.
struct ValidatedDraw {
    plan: DrawPlan,
    vertex_entry_index: usize,
}

/// The `PassStage::Draw` half of pass validation (ADR-0171), in check
/// order: the vertex entry exists, the geometry slot the dispatch fills
/// is declared, the vertex stage's interface agrees with that slot's
/// layout, and the depth declaration is coherent with the pass.
///
/// The depth rule: a pass depth-tests exactly when it names a depth
/// slot, so the only two ways to be wrong are naming a slot that does
/// not exist or resolve to the color output's extent (wgpu requires
/// attachments of one size), and writing `@builtin(frag_depth)` from
/// the fragment stage with no depth attachment to write it into.
fn validate_draw(
    mail: &ProgramRegister,
    module: &Module,
    index: usize,
    draw: &DrawPass,
    fragment_entry: (usize, &str),
    output_extent: SlotExtent,
) -> Result<ValidatedDraw, String> {
    let (fragment_entry_index, fragment_entry_point) = fragment_entry;
    let vertex_entry_index = module
        .entry_points
        .iter()
        .position(|entry| entry.stage == ShaderStage::Vertex && entry.name == draw.vertex_entry_point)
        .ok_or_else(|| {
            format!("pass {index}: no vertex entry point named `{}` in the module", draw.vertex_entry_point)
        })?;

    let slot = mail.geometries.get(draw.geometry as usize).ok_or_else(|| {
        format!("pass {index}: geometry slot {} is out of range ({} declared)", draw.geometry, mail.geometries.len())
    })?;
    check_vertex_interface(module, index, vertex_entry_index, draw.geometry, &slot.layout)?;

    if let Some(depth) = draw.depth {
        let extent = *mail.depth_transients.get(depth as usize).ok_or_else(|| {
            format!("pass {index}: depth transient {depth} is out of range ({} declared)", mail.depth_transients.len())
        })?;
        if extent != output_extent {
            return Err(format!(
                "pass {index}: depth transient {depth} declares extent {extent:?}, which does not match its color \
                 output's extent {output_extent:?} — a depth attachment must be the size of the color attachment \
                 it tests for",
            ));
        }
    } else if writes_frag_depth(module, fragment_entry_index) {
        return Err(format!(
            "pass {index}: entry point `{fragment_entry_point}` writes @builtin(frag_depth), so the pass must \
             declare a depth transient to write it into",
        ));
    }

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

/// Check the vertex stage's declared interface against the geometry
/// slot's layout through naga's reflection: every `@location` the stage
/// reads must be declared by the layout, and its WGSL type must be the
/// one that location's format is consumed as. A layout attribute the
/// stage ignores is fine — the vertex buffer supplies it, and nothing
/// reads it.
fn check_vertex_interface(
    module: &Module,
    index: usize,
    vertex_entry_index: usize,
    geometry: u32,
    layout: &[VertexAttribute],
) -> Result<(), String> {
    for (location, ty) in entry_input_locations(module, vertex_entry_index) {
        let Some(attribute) = layout.iter().find(|attribute| attribute.location == location) else {
            return Err(format!(
                "pass {index}: the vertex stage reads @location({location}), which geometry slot {geometry}'s layout \
                 does not declare",
            ));
        };
        if !consumes_format(&module.types[ty].inner, attribute.format) {
            return Err(format!(
                "pass {index}: the vertex stage reads @location({location}) as {}, but geometry slot {geometry} \
                 declares it {:?}, which is consumed as {}",
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
        return matches!(binding, Binding::BuiltIn(BuiltIn::FragDepth));
    }
    match &module.types[result.ty].inner {
        TypeInner::Struct { members, .. } => {
            members.iter().any(|member| matches!(member.binding, Some(Binding::BuiltIn(BuiltIn::FragDepth))))
        }
        _ => false,
    }
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

fn resolve_input(
    mail: &ProgramRegister,
    earlier: &[PassPlan],
    transients: &[TransientPlan],
    index: usize,
    input_index: usize,
    input: InputSlot,
) -> Result<ResolvedSlot, String> {
    match input {
        InputSlot::Binding { index: binding } => {
            check_binding_index(mail, index, binding)?;
            Ok(ResolvedSlot::Binding(binding))
        }
        InputSlot::PassOutput { pass } => earlier
            .get(pass as usize)
            .map(|prior| prior.output)
            .ok_or_else(|| format!("pass {index} reads the output of pass {pass}, which does not run before it")),
        InputSlot::Transient { index: transient } => {
            check_transient_index(mail, index, transient)?;
            if transients[transient as usize].first_write.is_none() {
                return Err(format!(
                    "pass {index} input {input_index} reads transient {transient} before any earlier pass writes it",
                ));
            }
            Ok(ResolvedSlot::Transient(transient))
        }
    }
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
    use crate::PassRepeat;

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
            entry_point: entry.to_owned(),
            inputs,
            output,
            uniform_offset: offset,
            uniform_length: length,
            repeat: None,
        }
    }

    fn full(format: TextureFormat) -> SlotSpec {
        SlotSpec { format, extent: SlotExtent::Full }
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
            transients: vec![full(TextureFormat::Rgba8); 3],
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
            transients: vec![full(TextureFormat::Rgba8)],
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
            transients: vec![full(TextureFormat::Rgba8)],
            passes: vec![ProgramPass { output: OutputSlot::Transient { index: 0 }, ..valid_pass() }],
            ..base()
        });
        assert!(transient_tail.contains("final pass"), "final-output class: {transient_tail}");

        let zero_repeat = rejection(&ProgramRegister {
            passes: vec![ProgramPass { repeat: Some(PassRepeat { count: 0, uniform_stride: 0 }), ..valid_pass() }],
            ..base()
        });
        assert!(zero_repeat.contains("repeat count"), "repeat class: {zero_repeat}");
    }

    /// A draw-pass module: one entry per shape the draw validation has
    /// to see. `vs_flat` reads position alone and takes its clip depth
    /// from the uniform window (the vertex stage reading group 0 is what
    /// makes the window's visibility load-bearing); `vs_tinted` reads a
    /// second location; `fs_depth_writer` writes `@builtin(frag_depth)`.
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
        let draw_pass = || ProgramPass {
            stage: draw_stage("vs_flat", 0, Some(0)),
            entry_point: "fs_flat".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: DRAW_PARAMS_BYTES,
            repeat: None,
        };
        let base = || ProgramRegister {
            wgsl: DRAW_MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8)],
            transients: Vec::new(),
            geometries: vec![position_slot()],
            depth_transients: vec![SlotExtent::Full],
            passes: vec![draw_pass()],
        };

        let plan = validate(&base()).expect("the baseline draw program validates");
        let drawn = plan.passes[0].draw.as_ref().expect("the draw pass carries a draw plan");
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

        let mismatched_depth =
            rejection(&ProgramRegister { depth_transients: vec![SlotExtent::Divided { divisor: 2 }], ..base() });
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
                inputs: Vec::new(),
                output: OutputSlot::Binding { index: 0 },
                uniform_offset: 0,
                uniform_length: 4,
                repeat: None,
            }],
        };
        let short = rejection(&mail);
        assert!(short.contains("uniform window"), "window class over the vertex stage: {short}");
    }
}
