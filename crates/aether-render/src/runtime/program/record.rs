//! Dispatch-time execution of a registered program (ADR-0170): resolve
//! the dispatch's bindings against the validated plan (every mismatch
//! warn-drops naming the program, pass, and binding — the same
//! convention as an unknown texture id in `draw_textured_quads`), stage
//! the uniform windows into an aligned arrangement, acquire pooled
//! transients, and record the passes into the frame encoder.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use aether_substrate::render::{ProgramPassDraw, create_program_transient, record_program_pass};

use super::super::pipeline::RenderGpu;
use super::super::texture::{TextureRegistry, wgpu_texture_format};
use super::validate::{ProgramPlan, ResolvedSlot, resolve_extent};
use super::{RegisteredProgram, TransientKey};
use crate::{ProgramDispatch, TextureSampling, TextureUsage};

/// One transient's physical allocation for a dispatch: which pool class
/// it draws from and which slot within that class it occupies.
struct TransientAssignment {
    key: TransientKey,
    physical: usize,
}

/// Execute one dispatch into `encoder`, or warn-drop it whole: the
/// checks run first, so a rejected dispatch records nothing and the
/// frame survives untouched.
pub(super) fn record_dispatch(
    gpu: &RenderGpu,
    encoder: &mut wgpu::CommandEncoder,
    program: &RegisteredProgram,
    pool: &mut HashMap<TransientKey, Vec<wgpu::TextureView>>,
    textures: &mut TextureRegistry,
    dispatch: &ProgramDispatch,
) {
    let plan = &program.plan;
    let Some(reference) = check_dispatch(plan, textures, dispatch) else {
        return;
    };

    for &texture_id in &dispatch.bindings {
        if let Some(entry) = textures.entries.get_mut(&texture_id) {
            entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
        }
    }
    let assignments = assign_transients(plan, reference);
    for assignment in assignments.iter().flatten() {
        let views = pool.entry(assignment.key).or_default();
        while views.len() <= assignment.physical {
            let (width, height, format) = assignment.key;
            views.push(
                create_program_transient(&gpu.device, width, height, format)
                    .create_view(&wgpu::TextureViewDescriptor::default()),
            );
        }
    }

    encode_passes(gpu, encoder, program, pool, textures, dispatch, &assignments);
}

/// Run every dispatch-time check, warn-dropping on the first mismatch.
/// Returns the program's reference extent (the output binding texture's
/// size) on success.
fn check_dispatch(plan: &ProgramPlan, textures: &TextureRegistry, dispatch: &ProgramDispatch) -> Option<(u32, u32)> {
    let program_id = dispatch.program_id;
    if dispatch.bindings.len() != plan.bindings.len() {
        tracing::warn!(
            target: "aether_render",
            program_id,
            declared = plan.bindings.len(),
            supplied = dispatch.bindings.len(),
            "program dispatch binding count disagrees with the registered graph; dropping the dispatch",
        );
        return None;
    }

    for (binding, &texture_id) in dispatch.bindings.iter().enumerate() {
        if !textures.entries.contains_key(&texture_id) {
            tracing::warn!(
                target: "aether_render",
                program_id,
                binding,
                texture_id,
                "program dispatch binding names an unknown texture id; dropping the dispatch",
            );
            return None;
        }
    }
    let reference = {
        let entry = &textures.entries[&dispatch.bindings[plan.output_binding as usize]];
        (entry.width, entry.height)
    };
    for (binding, &texture_id) in dispatch.bindings.iter().enumerate() {
        let entry = &textures.entries[&texture_id];
        let spec = plan.bindings[binding];
        if entry.format != spec.format {
            tracing::warn!(
                target: "aether_render",
                program_id,
                binding,
                texture_id,
                declared = ?spec.format,
                bound = ?entry.format,
                "program dispatch binding format disagrees with the registered graph; dropping the dispatch",
            );
            return None;
        }
        let expected = resolve_extent(spec.extent, reference);
        if (entry.width, entry.height) != expected {
            tracing::warn!(
                target: "aether_render",
                program_id,
                binding,
                texture_id,
                expected_width = expected.0,
                expected_height = expected.1,
                bound_width = entry.width,
                bound_height = entry.height,
                "program dispatch binding size disagrees with the registered graph; dropping the dispatch",
            );
            return None;
        }
    }
    for &binding in &plan.written_bindings {
        let texture_id = dispatch.bindings[binding as usize];
        if textures.entries[&texture_id].usage != TextureUsage::Writable {
            tracing::warn!(
                target: "aether_render",
                program_id,
                binding = binding as usize,
                texture_id,
                "program dispatch binds a non-writable texture where the graph writes; dropping the dispatch",
            );
            return None;
        }
    }

    for (pass, pass_plan) in plan.passes.iter().enumerate() {
        if pass_plan.uniform_length > 0 {
            let end = u64::from(pass_plan.uniform_offset)
                + u64::from(pass_plan.repeat_count - 1) * u64::from(pass_plan.uniform_stride)
                + u64::from(pass_plan.uniform_length);
            if end > dispatch.uniforms.len() as u64 {
                tracing::warn!(
                    target: "aether_render",
                    program_id,
                    pass,
                    needed_bytes = end,
                    blob_bytes = dispatch.uniforms.len(),
                    "program dispatch uniform blob is shorter than a pass's window; dropping the dispatch",
                );
                return None;
            }
        }
        if let ResolvedSlot::Binding(output) = pass_plan.output {
            let output_id = dispatch.bindings[output as usize];
            for input in &pass_plan.inputs {
                if let ResolvedSlot::Binding(input_binding) = input
                    && dispatch.bindings[*input_binding as usize] == output_id
                {
                    tracing::warn!(
                        target: "aether_render",
                        program_id,
                        pass,
                        binding = *input_binding as usize,
                        texture_id = output_id,
                        "program dispatch binds one texture as both a pass input and its output; \
                         dropping the dispatch",
                    );
                    return None;
                }
            }
        }
    }
    Some(reference)
}

/// Copy each pass iteration's uniform window into an offset-aligned
/// staging arrangement (wgpu dynamic uniform offsets must be multiples
/// of `min_uniform_buffer_offset_alignment`, and dispatch windows carry
/// no alignment of their own), upload it once, and record the passes.
fn encode_passes(
    gpu: &RenderGpu,
    encoder: &mut wgpu::CommandEncoder,
    program: &RegisteredProgram,
    pool: &HashMap<TransientKey, Vec<wgpu::TextureView>>,
    textures: &TextureRegistry,
    dispatch: &ProgramDispatch,
    assignments: &[Option<TransientAssignment>],
) {
    let plan = &program.plan;
    let align = usize::try_from(gpu.device.limits().min_uniform_buffer_offset_alignment)
        .expect("uniform offset alignment fits usize")
        .max(4);
    let mut staging: Vec<u8> = Vec::new();
    let mut iteration_offsets: Vec<Vec<u32>> = Vec::with_capacity(plan.passes.len());
    for (pass_plan, pass_gpu) in plan.passes.iter().zip(&program.passes_gpu) {
        let bound = usize::try_from(pass_gpu.bound_uniform_bytes).expect("bound uniform bytes fit usize");
        let mut offsets = Vec::with_capacity(pass_plan.repeat_count as usize);
        for iteration in 0..pass_plan.repeat_count as usize {
            let aligned = staging.len().next_multiple_of(align);
            staging.resize(aligned, 0);
            if pass_plan.uniform_length > 0 {
                let start = pass_plan.uniform_offset as usize + iteration * pass_plan.uniform_stride as usize;
                staging.extend_from_slice(&dispatch.uniforms[start..start + pass_plan.uniform_length as usize]);
            }
            // A zero-length window stages `bound` zero bytes (the dummy
            // binding); a nonzero one pads its tail up to `bound`.
            staging.resize(aligned + bound, 0);
            offsets.push(u32::try_from(aligned).expect("staged uniform offset fits u32"));
        }
        iteration_offsets.push(offsets);
    }
    staging.resize(staging.len().next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT as usize), 0);

    let uniform_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("aether program uniform staging"),
        size: staging.len() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    gpu.queue.write_buffer(&uniform_buffer, 0, &staging);

    // Views over the realized binding textures, one per distinct id —
    // both the sampled inputs and the written attachments read from
    // this map.
    let mut binding_views: HashMap<u32, wgpu::TextureView> = HashMap::new();
    for &texture_id in &dispatch.bindings {
        binding_views.entry(texture_id).or_insert_with(|| {
            textures.entries[&texture_id]
                .realized
                .as_ref()
                .expect("realized before encode")
                .texture()
                .create_view(&wgpu::TextureViewDescriptor::default())
        });
    }
    let transient_view = |transient: u32| {
        let assignment =
            assignments[transient as usize].as_ref().expect("read/written transients were assigned physical slots");
        &pool[&assignment.key][assignment.physical]
    };

    let mut written: Vec<ResolvedSlot> = Vec::new();
    for ((pass_plan, pass_gpu), offsets) in plan.passes.iter().zip(&program.passes_gpu).zip(&iteration_offsets) {
        let uniform_bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("aether program uniform bind group"),
            layout: &pass_gpu.uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &uniform_buffer,
                    offset: 0,
                    size: wgpu::BufferSize::new(pass_gpu.bound_uniform_bytes),
                }),
            }],
        });

        let mut input_entries = Vec::with_capacity(pass_plan.inputs.len() * 2);
        for (input, slot) in pass_plan.inputs.iter().enumerate() {
            let (view, nearest) = match slot {
                ResolvedSlot::Binding(binding) => {
                    let texture_id = dispatch.bindings[*binding as usize];
                    let entry = &textures.entries[&texture_id];
                    let nearest = entry.sampling == TextureSampling::Nearest || !entry.format.filterable();
                    (&binding_views[&texture_id], nearest)
                }
                ResolvedSlot::Transient(transient) => {
                    (transient_view(*transient), !plan.slot_format(*slot).filterable())
                }
            };
            let sampler = if nearest {
                &gpu.texture_bindings.nearest_sampler
            } else {
                &gpu.texture_bindings.sampler
            };
            let base = u32::try_from(input * 2).expect("program input binding index fits u32");
            input_entries
                .push(wgpu::BindGroupEntry { binding: base, resource: wgpu::BindingResource::TextureView(view) });
            input_entries
                .push(wgpu::BindGroupEntry { binding: base + 1, resource: wgpu::BindingResource::Sampler(sampler) });
        }
        let inputs_bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("aether program inputs bind group"),
            layout: &pass_gpu.inputs_layout,
            entries: &input_entries,
        });

        let target_view = match pass_plan.output {
            ResolvedSlot::Binding(binding) => &binding_views[&dispatch.bindings[binding as usize]],
            ResolvedSlot::Transient(transient) => transient_view(transient),
        };
        for &uniform_offset in offsets {
            let clear = !written.contains(&pass_plan.output);
            if clear {
                written.push(pass_plan.output);
            }
            record_program_pass(
                encoder,
                &ProgramPassDraw {
                    pipeline: &pass_gpu.pipeline,
                    target_view,
                    clear,
                    uniform_bind_group: &uniform_bind_group,
                    uniform_offset,
                    inputs_bind_group: &inputs_bind_group,
                },
            );
        }
    }
}

/// Assign each live transient a physical texture in its resolved
/// (extent, format) class, reusing a physical slot once its previous
/// holder's live range has ended — strictly before the next holder's
/// first write, so a pass never samples a transient the same physical
/// texture is attached to. A ping-pong chain of any length settles on
/// two allocations per class.
fn assign_transients(plan: &ProgramPlan, reference: (u32, u32)) -> Vec<Option<TransientAssignment>> {
    let mut order: Vec<usize> =
        (0..plan.transients.len()).filter(|&t| plan.transients[t].first_write.is_some()).collect();
    order.sort_by_key(|&t| plan.transients[t].first_write.expect("filtered to written transients"));

    let mut free: HashMap<TransientKey, BinaryHeap<Reverse<(u32, usize)>>> = HashMap::new();
    let mut allocated: HashMap<TransientKey, usize> = HashMap::new();
    let mut assignments: Vec<Option<TransientAssignment>> = (0..plan.transients.len()).map(|_| None).collect();
    for t in order {
        let live = &plan.transients[t];
        let first_write = live.first_write.expect("filtered to written transients");
        let last_use = live.last_use.expect("a written transient has a live range");
        let (width, height) = resolve_extent(live.spec.extent, reference);
        let key = (width, height, wgpu_texture_format(live.spec.format));

        let heap = free.entry(key).or_default();
        let physical = match heap.peek() {
            Some(Reverse((available_from, physical))) if *available_from <= first_write => {
                let physical = *physical;
                heap.pop();
                physical
            }
            _ => {
                let count = allocated.entry(key).or_insert(0);
                let physical = *count;
                *count += 1;
                physical
            }
        };
        free.entry(key).or_default().push(Reverse((last_use + 1, physical)));
        assignments[t] = Some(TransientAssignment { key, physical });
    }
    assignments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InputSlot, OutputSlot, PassStage, ProgramPass, ProgramRegister, SlotExtent, SlotSpec, TextureFormat};

    const MODULE: &str = r"
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@fragment
fn fs_copy(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, uv);
}
";

    fn copy_pass(input: InputSlot, output: OutputSlot) -> ProgramPass {
        ProgramPass {
            stage: PassStage::Fragment,
            entry_point: "fs_copy".to_owned(),
            inputs: vec![input],
            output,
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }
    }

    /// The interval reuse behind the ADR's pooling claim: a chain of
    /// hops through same-class transients settles on two physical
    /// allocations, and reuse begins strictly after the prior holder's
    /// last read. The named bugs: an `available_from <= first_write`
    /// off-by-one that hands a still-live transient's texture to the
    /// pass sampling it (chain corruption), or a policy that never
    /// reuses and allocates one texture per hop.
    #[test]
    fn ping_pong_chain_settles_on_two_physical_allocations() {
        let full = SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full };
        let mail = ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full, full],
            transients: vec![full; 3],
            passes: vec![
                copy_pass(InputSlot::Binding { index: 0 }, OutputSlot::Transient { index: 0 }),
                copy_pass(InputSlot::Transient { index: 0 }, OutputSlot::Transient { index: 1 }),
                copy_pass(InputSlot::Transient { index: 1 }, OutputSlot::Transient { index: 2 }),
                copy_pass(InputSlot::Transient { index: 2 }, OutputSlot::Binding { index: 1 }),
            ],
        };
        let plan = super::super::validate::validate(&mail).expect("chain validates");

        let assignments = assign_transients(&plan, (64, 48));
        let physicals: Vec<usize> =
            assignments.iter().map(|a| a.as_ref().expect("all transients live").physical).collect();
        // Transient 0 is read at pass 1 while transient 1 is written, so
        // they must differ; transient 0's range ends at pass 1, freeing
        // its texture for transient 2's write at pass 2.
        assert_eq!(physicals, vec![0, 1, 0]);
    }
}
