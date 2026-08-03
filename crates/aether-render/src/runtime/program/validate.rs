//! Register-time program validation (ADR-0170): the WGSL through naga,
//! then the declared graph — pure CPU, no device. Success produces the
//! [`ProgramPlan`] the register path builds pipelines from and the
//! dispatch path records against; failure produces the `Err { reason }`
//! string, one distinguishable message per validation class.

use crate::{InputSlot, OutputSlot, PassStage, ProgramPass, ProgramRegister, SlotExtent, SlotSpec, TextureFormat};

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
pub struct PassPlan {
    pub entry_point: String,
    pub inputs: Vec<ResolvedSlot>,
    pub output: ResolvedSlot,
    pub uniform_offset: u32,
    pub uniform_length: u32,
    pub repeat_count: u32,
    pub uniform_stride: u32,
}

/// One transient's declaration plus its live range over the pass
/// sequence — `first_write..=last_use`, `None` when no pass references
/// it. The dispatch-time pool assignment reuses a physical texture
/// whose previous holder's `last_use` lies strictly before the next
/// holder's `first_write`.
pub struct TransientPlan {
    pub spec: SlotSpec,
    pub first_write: Option<u32>,
    pub last_use: Option<u32>,
}

/// The validated program graph: everything the register path needs to
/// build pipelines and the dispatch path needs to resolve and record.
pub struct ProgramPlan {
    pub bindings: Vec<SlotSpec>,
    pub transients: Vec<TransientPlan>,
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
    let module = naga::front::wgsl::parse_str(&mail.wgsl)
        .map_err(|error| format!("invalid wgsl: {}", error.emit_to_string(&mail.wgsl)))?;
    let info = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
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

    let mut transients: Vec<TransientPlan> =
        mail.transients.iter().map(|spec| TransientPlan { spec: *spec, first_write: None, last_use: None }).collect();
    let mut passes: Vec<PassPlan> = Vec::with_capacity(mail.passes.len());
    let mut written_bindings: Vec<u32> = Vec::new();
    for (index, pass) in mail.passes.iter().enumerate() {
        let plan = validate_pass(mail, &module, &info, &passes, &transients, index, pass)?;
        if let ResolvedSlot::Transient(transient) = plan.output {
            let live = &mut transients[transient as usize];
            live.first_write.get_or_insert(index as u32);
            live.last_use = Some(index as u32);
        }
        for input in &plan.inputs {
            if let ResolvedSlot::Transient(transient) = input {
                transients[*transient as usize].last_use = Some(index as u32);
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

    Ok(ProgramPlan { bindings: mail.bindings.clone(), transients, passes, output_binding, written_bindings })
}

fn check_extent(extent: SlotExtent, slot: impl Fn() -> String) -> Result<(), String> {
    match extent {
        SlotExtent::Divided { divisor: 0 } => Err(format!("{}: extent divisor must be at least 1", slot())),
        SlotExtent::Full | SlotExtent::Divided { .. } => Ok(()),
    }
}

// One linear walk per pass — entry point, inputs, output, window,
// repeat — reads better in sequence than split into per-check helpers
// that would each re-thread the same five context arguments.
#[allow(clippy::too_many_arguments)]
fn validate_pass(
    mail: &ProgramRegister,
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    earlier: &[PassPlan],
    transients: &[TransientPlan],
    index: usize,
    pass: &ProgramPass,
) -> Result<PassPlan, String> {
    // `PassStage` has one variant, so nothing to check yet; destructure
    // so a future `Compute` arm fails to compile here rather than
    // silently building a fragment pipeline.
    let PassStage::Fragment = pass.stage;

    let entry_index = module
        .entry_points
        .iter()
        .position(|entry| entry.stage == naga::ShaderStage::Fragment && entry.name == pass.entry_point)
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

    if let Some(block_bytes) = uniform_block_bytes(module, info, entry_index)
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
        inputs,
        output,
        uniform_offset: pass.uniform_offset,
        uniform_length: pass.uniform_length,
        repeat_count,
        uniform_stride,
    })
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
fn uniform_block_bytes(module: &naga::Module, info: &naga::valid::ModuleInfo, entry_index: usize) -> Option<u32> {
    let entry_info = info.get_entry_point(entry_index);
    module
        .global_variables
        .iter()
        .find(|(handle, var)| {
            matches!(var.space, naga::AddressSpace::Uniform)
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
    #[test]
    fn validation_classes_have_distinguishable_reasons() {
        let valid_pass =
            || pass("fs_copy", vec![InputSlot::Binding { index: 0 }], OutputSlot::Binding { index: 1 }, 0, 4);
        let base = || ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8)],
            transients: vec![],
            passes: vec![valid_pass()],
        };

        let bad_wgsl = validate(&ProgramRegister { wgsl: "not wgsl at all".to_owned(), ..base() }).unwrap_err();
        assert!(bad_wgsl.starts_with("invalid wgsl:"), "naga class: {bad_wgsl}");

        let missing_entry = validate(&ProgramRegister {
            passes: vec![ProgramPass { entry_point: "fs_missing".to_owned(), ..valid_pass() }],
            ..base()
        })
        .unwrap_err();
        assert!(missing_entry.contains("no fragment entry point"), "entry class: {missing_entry}");

        let unwritten_read = validate(&ProgramRegister {
            transients: vec![full(TextureFormat::Rgba8)],
            passes: vec![ProgramPass { inputs: vec![InputSlot::Transient { index: 0 }], ..valid_pass() }],
            ..base()
        })
        .unwrap_err();
        assert!(unwritten_read.contains("before any earlier pass writes it"), "sequence class: {unwritten_read}");

        let short_window =
            validate(&ProgramRegister { passes: vec![ProgramPass { uniform_length: 2, ..valid_pass() }], ..base() })
                .unwrap_err();
        assert!(short_window.contains("uniform window"), "window class: {short_window}");

        let self_read = validate(&ProgramRegister {
            passes: vec![ProgramPass { inputs: vec![InputSlot::Binding { index: 1 }], ..valid_pass() }],
            ..base()
        })
        .unwrap_err();
        assert!(self_read.contains("its own output"), "self-read class: {self_read}");

        let transient_tail = validate(&ProgramRegister {
            transients: vec![full(TextureFormat::Rgba8)],
            passes: vec![ProgramPass { output: OutputSlot::Transient { index: 0 }, ..valid_pass() }],
            ..base()
        })
        .unwrap_err();
        assert!(transient_tail.contains("final pass"), "final-output class: {transient_tail}");

        let zero_repeat = validate(&ProgramRegister {
            passes: vec![ProgramPass { repeat: Some(PassRepeat { count: 0, uniform_stride: 0 }), ..valid_pass() }],
            ..base()
        })
        .unwrap_err();
        assert!(zero_repeat.contains("repeat count"), "repeat class: {zero_repeat}");
    }
}
