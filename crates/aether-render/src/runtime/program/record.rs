//! Dispatch-time execution of a registered program (ADR-0170): resolve
//! the dispatch's bindings against the validated plan (every mismatch
//! warn-drops naming the program, pass, and binding — the same
//! convention as an unknown texture id in `draw_textured_quads`), stage
//! the uniform windows into an aligned arrangement, acquire pooled
//! transients, and record the passes into the frame encoder.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use aether_substrate::render::{
    PROGRAM_DEPTH_FORMAT, ProgramDepthAttachment, ProgramDrawPass, ProgramPassDraw, create_program_depth_transient,
    create_program_transient, record_program_draw_pass, record_program_pass,
};

use super::super::geometry::GeometryRegistry;
use super::super::pipeline::RenderGpu;
use super::super::texture::{TextureRegistry, wgpu_texture_format};
use super::validate::{ProgramPlan, ResolvedSlot, resolve_extent};
use super::{RegisteredProgram, TransientKey};
use crate::{PassLoad, ProgramDispatch, TextureSampling, TextureUsage};

/// One transient's physical allocation for a dispatch: which pool class
/// it draws from and which slot within that class it occupies.
struct TransientAssignment {
    key: TransientKey,
    physical: usize,
}

/// Execute one dispatch into `encoder`, or warn-drop it whole: the
/// checks run first, so a rejected dispatch records nothing and the
/// frame survives untouched.
// The realize / pool / encode sequence takes the two registries plus the
// pool and the dispatch; threading them through a bundle struct for the
// one call site would only rename the same borrows.
#[allow(clippy::too_many_arguments)]
pub(super) fn record_dispatch(
    gpu: &RenderGpu,
    encoder: &mut wgpu::CommandEncoder,
    program: &RegisteredProgram,
    pool: &mut HashMap<TransientKey, Vec<wgpu::TextureView>>,
    textures: &mut TextureRegistry,
    geometries: &mut GeometryRegistry,
    dispatch: &ProgramDispatch,
) {
    let plan = &program.plan;
    let Some(reference) = check_dispatch(plan, textures, geometries, dispatch) else {
        return;
    };

    for &texture_id in &dispatch.bindings {
        if let Some(entry) = textures.entries.get_mut(&texture_id) {
            entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
        }
    }
    for pass_plan in &plan.passes {
        if let Some(draw) = &pass_plan.draw
            && let Some(entry) = geometries.entries.get_mut(&dispatch.geometries[draw.geometry as usize])
        {
            entry.ensure_realized(&gpu.device, &gpu.queue);
        }
    }
    let assignments = assign_transients(plan, reference);
    let depth_assignments = assign_depth_transients(plan, reference);
    for assignment in assignments.iter().chain(depth_assignments.iter()).flatten() {
        let views = pool.entry(assignment.key).or_default();
        while views.len() <= assignment.physical {
            let (width, height, format) = assignment.key;
            let texture = if format == PROGRAM_DEPTH_FORMAT {
                create_program_depth_transient(&gpu.device, width, height)
            } else {
                create_program_transient(&gpu.device, width, height, format)
            };
            views.push(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        }
    }

    encode_passes(gpu, encoder, program, pool, textures, geometries, dispatch, &assignments, &depth_assignments);
}

/// Run every dispatch-time check, warn-dropping on the first mismatch.
/// Returns the program's reference extent (the output binding texture's
/// size) on success.
// One check per warn-drop class in dispatch order; the line count is the
// tracing fields naming program, pass, and binding, and splitting per
// class would scatter the single rejection narrative.
#[allow(clippy::too_many_lines)]
fn check_dispatch(
    plan: &ProgramPlan,
    textures: &TextureRegistry,
    geometries: &GeometryRegistry,
    dispatch: &ProgramDispatch,
) -> Option<(u32, u32)> {
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
    if dispatch.geometries.len() != plan.geometries.len() {
        tracing::warn!(
            target: "aether_render",
            program_id,
            declared = plan.geometries.len(),
            supplied = dispatch.geometries.len(),
            "program dispatch geometry count disagrees with the registered graph; dropping the dispatch",
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
        if let Some(draw) = &pass_plan.draw {
            let binding = draw.geometry as usize;
            let geometry_id = dispatch.geometries[binding];
            let Some(entry) = geometries.entries.get(&geometry_id) else {
                tracing::warn!(
                    target: "aether_render",
                    program_id,
                    pass,
                    binding,
                    geometry_id,
                    "program dispatch geometry binding names an unknown geometry id; dropping the dispatch",
                );
                return None;
            };
            if entry.layout != plan.geometries[binding].layout {
                tracing::warn!(
                    target: "aether_render",
                    program_id,
                    pass,
                    binding,
                    geometry_id,
                    declared = ?plan.geometries[binding].layout,
                    bound = ?entry.layout,
                    "program dispatch geometry layout disagrees with the registered graph; dropping the dispatch",
                );
                return None;
            }
        }
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
// Staging, bind groups, and pass encoding share the per-pass borrow
// structure; splitting them would re-thread the same context
// arguments — the same shape `record_overlay_batches` keeps.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn encode_passes(
    gpu: &RenderGpu,
    encoder: &mut wgpu::CommandEncoder,
    program: &RegisteredProgram,
    pool: &HashMap<TransientKey, Vec<wgpu::TextureView>>,
    textures: &TextureRegistry,
    geometries: &GeometryRegistry,
    dispatch: &ProgramDispatch,
    assignments: &[Option<TransientAssignment>],
    depth_assignments: &[Option<TransientAssignment>],
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
    let copy_alignment = usize::try_from(wgpu::COPY_BUFFER_ALIGNMENT).expect("copy alignment fits usize");
    staging.resize(staging.len().next_multiple_of(copy_alignment), 0);

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
    let depth_view = |slot: u32| {
        let assignment =
            depth_assignments[slot as usize].as_ref().expect("depth slots a pass names were assigned physical slots");
        &pool[&assignment.key][assignment.physical]
    };

    let mut written: Vec<ResolvedSlot> = Vec::new();
    let mut depth_cleared: Vec<u32> = Vec::new();
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
        for (iteration, &uniform_offset) in offsets.iter().enumerate() {
            let first_write = !written.contains(&pass_plan.output);
            if first_write {
                written.push(pass_plan.output);
            }
            let Some(draw) = &pass_plan.draw else {
                record_program_pass(
                    encoder,
                    &ProgramPassDraw {
                        pipeline: &pass_gpu.pipeline,
                        target_view,
                        clear: first_write,
                        uniform_bind_group: &uniform_bind_group,
                        uniform_offset,
                        inputs_bind_group: &inputs_bind_group,
                    },
                );
                continue;
            };

            // A draw pass's declared load is authoritative on its color
            // output; its later repeat iterations load so a repeat
            // accumulates rather than each iteration wiping the last.
            // Depth follows the shared-slot rule: the dispatch's first
            // reference to a slot clears it, later ones load it.
            let depth = draw.depth.map(|slot| {
                let clear = !depth_cleared.contains(&slot);
                if clear {
                    depth_cleared.push(slot);
                }
                ProgramDepthAttachment { view: depth_view(slot), clear }
            });
            let realized = geometries.entries[&dispatch.geometries[draw.geometry as usize]]
                .realized
                .as_ref()
                .expect("realized before encode");
            record_program_draw_pass(
                encoder,
                &ProgramDrawPass {
                    pipeline: &pass_gpu.pipeline,
                    target_view,
                    clear_color: draw.load == PassLoad::Clear && iteration == 0,
                    depth,
                    uniform_bind_group: &uniform_bind_group,
                    uniform_offset,
                    inputs_bind_group: &inputs_bind_group,
                    vertex_buffer: &realized.vertex_buffer,
                    index_buffer: &realized.index_buffer,
                    index_count: realized.index_count,
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

/// Assign each depth slot a physical `Depth32Float` texture in its
/// resolved-extent class, skipping slots no pass names. Unlike color
/// transients these are not liveness-packed: sharing a depth buffer is
/// the declaration's whole point (two passes naming one slot is how
/// occlusion agrees between them), so two *distinct* slots must never
/// land on one physical texture however disjoint their use looks.
fn assign_depth_transients(plan: &ProgramPlan, reference: (u32, u32)) -> Vec<Option<TransientAssignment>> {
    let mut allocated: HashMap<TransientKey, usize> = HashMap::new();
    plan.depth_transients
        .iter()
        .enumerate()
        .map(|(slot, extent)| {
            let slot = u32::try_from(slot).expect("depth slot index fits u32");
            let named = plan.passes.iter().any(|pass| pass.draw.as_ref().is_some_and(|draw| draw.depth == Some(slot)));
            named.then(|| {
                let (width, height) = resolve_extent(*extent, reference);
                let key = (width, height, PROGRAM_DEPTH_FORMAT);
                let physical = allocated.entry(key).or_insert(0);
                let assigned = *physical;
                *physical += 1;
                TransientAssignment { key, physical: assigned }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DrawPass, GeometrySlotSpec, InputSlot, OutputSlot, PassStage, ProgramPass, ProgramRegister, SlotExtent,
        SlotSpec, TextureFormat, VertexAttribute, VertexFormat,
    };

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
            geometries: Vec::new(),
            depth_transients: Vec::new(),
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

    const DRAW_MODULE: &str = r"
@vertex
fn vs_flat(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position, 1.0);
}

@fragment
fn fs_opaque() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}
";

    fn draw_pass(depth: Option<u32>) -> ProgramPass {
        ProgramPass {
            stage: PassStage::Draw(DrawPass {
                vertex_entry_point: "vs_flat".to_owned(),
                geometry: 0,
                depth,
                load: PassLoad::Clear,
            }),
            entry_point: "fs_opaque".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }
    }

    /// ADR-0171 depth pooling: two passes naming one depth slot share
    /// one physical texture (that sharing is what makes their occlusion
    /// agree), two *distinct* slots never do however disjoint their use
    /// looks, and a declared slot no pass names allocates nothing. The
    /// named bugs: liveness-packing depth the way color transients are
    /// packed, which would alias two slots onto one buffer and let one
    /// pass's depth occlude another's geometry; and a named slot left
    /// unassigned, which panics the encode path that resolves its view.
    #[test]
    fn depth_slots_share_by_name_and_never_alias() {
        let full = SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full };
        let mail = ProgramRegister {
            wgsl: DRAW_MODULE.to_owned(),
            bindings: vec![full],
            transients: Vec::new(),
            geometries: vec![GeometrySlotSpec {
                layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }],
            }],
            depth_transients: vec![SlotExtent::Full; 3],
            passes: vec![draw_pass(Some(0)), draw_pass(Some(0)), draw_pass(Some(1))],
        };
        let plan = super::super::validate::validate(&mail).expect("draw graph validates");

        let assignments = assign_depth_transients(&plan, (64, 48));
        let physicals: Vec<Option<usize>> =
            assignments.iter().map(|slot| slot.as_ref().map(|assigned| assigned.physical)).collect();
        assert_eq!(physicals, vec![Some(0), Some(1), None]);
        assert_eq!(
            assignments[0].as_ref().expect("slot 0 named").key,
            assignments[1].as_ref().expect("slot 1 named").key,
            "same-extent depth slots share a pool class, so only the physical index may separate them",
        );
    }
}
