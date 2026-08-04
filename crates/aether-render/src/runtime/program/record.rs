//! Dispatch-time execution of a registered program (ADR-0170): resolve
//! the dispatch's bindings against the validated plan (every mismatch
//! warn-drops naming the program, pass, and binding — the same
//! convention as an unknown texture id in `draw_textured_quads`), stage
//! the uniform windows into an aligned arrangement, acquire pooled
//! transients, and record the passes into the frame encoder.
//!
//! The checks run against the dispatch every time. Everything after them
//! is derived state the program holds across dispatches in its dispatch
//! cache, because a steady-state repaint rebinds the same resources
//! every frame — see `super::cache` for what invalidates what.

use std::collections::HashMap;

use aether_substrate::render::{
    PROGRAM_DEPTH_FORMAT, ProgramDepthAttachment, ProgramDrawPass, ProgramPassDraw, create_program_depth_transient,
    create_program_transient, record_program_draw_pass, record_program_pass,
};

use super::super::geometry::GeometryRegistry;
use super::super::pipeline::RenderGpu;
use super::super::texture::TextureRegistry;
use super::cache::{BoundInput, CacheParts};
use super::timing::FrameQueries;
use super::validate::{ProgramPlan, ResolvedSlot, resolve_extent};
use super::{PassGpu, RegisteredProgram, TransientKey};
use crate::{PassLoad, ProgramDispatch, TextureSampling, TextureUsage};

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
    program: &mut RegisteredProgram,
    pool: &mut HashMap<TransientKey, Vec<wgpu::TextureView>>,
    textures: &mut TextureRegistry,
    geometries: &mut GeometryRegistry,
    dispatch: &ProgramDispatch,
    queries: Option<FrameQueries<'_>>,
) {
    let RegisteredProgram { plan, passes_gpu, cache, timings } = program;
    let Some(reference) = check_dispatch(plan, textures, geometries, dispatch) else {
        return;
    };
    timings.observe_reference(reference);

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

    let align = usize::try_from(gpu.device.limits().min_uniform_buffer_offset_alignment)
        .expect("uniform offset alignment fits usize")
        .max(4);
    cache.ensure_layout(plan, passes_gpu, align);
    cache.ensure_extent_layout(plan, reference);
    cache.refresh_binding_views(textures, &dispatch.bindings);

    let mut parts = cache.split();
    for assignment in parts.extent.assignments.iter().chain(parts.extent.depth_assignments.iter()).flatten() {
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

    encode_passes(gpu, encoder, plan, passes_gpu, &mut parts, pool, textures, geometries, dispatch, queries);
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

/// Stage the dispatch's uniform windows, refresh the bind groups whose
/// bound resources moved since the last dispatch, and record the passes.
/// In the steady state — the same bindings, the same extent — the loop
/// below allocates nothing and creates no GPU objects: it looks up the
/// two cached bind groups per pass and encodes.
// Staging, bind groups, and pass encoding share the per-pass borrow
// structure; splitting them would re-thread the same context
// arguments — the same shape `record_overlay_batches` keeps.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn encode_passes(
    gpu: &RenderGpu,
    encoder: &mut wgpu::CommandEncoder,
    plan: &ProgramPlan,
    passes_gpu: &[PassGpu],
    cache: &mut CacheParts<'_>,
    pool: &HashMap<TransientKey, Vec<wgpu::TextureView>>,
    textures: &TextureRegistry,
    geometries: &GeometryRegistry,
    dispatch: &ProgramDispatch,
    mut queries: Option<FrameQueries<'_>>,
) {
    cache.upload_uniforms(&gpu.device, &gpu.queue, plan, passes_gpu, &dispatch.uniforms);

    let layout = cache.layout;
    let extent = cache.extent;
    let transient_view = |transient: u32| {
        let assignment = extent.assignments[transient as usize]
            .as_ref()
            .expect("read/written transients were assigned physical slots");
        &pool[&assignment.key][assignment.physical]
    };
    let depth_view = |slot: u32| {
        let assignment = extent.depth_assignments[slot as usize]
            .as_ref()
            .expect("depth slots a pass names were assigned physical slots");
        &pool[&assignment.key][assignment.physical]
    };
    let bound_input = |slot: &ResolvedSlot| match slot {
        ResolvedSlot::Binding(binding) => BoundInput::Binding(dispatch.bindings[*binding as usize]),
        ResolvedSlot::Transient(transient) => {
            let assignment = extent.assignments[*transient as usize]
                .as_ref()
                .expect("read/written transients were assigned physical slots");
            BoundInput::Transient(assignment.key, assignment.physical)
        }
    };

    // Both reused across passes: neither borrows the cache, so they
    // survive the `&mut` that stores a rebuilt bind group.
    let mut input_key: Vec<BoundInput> = Vec::new();
    let mut input_entries: Vec<wgpu::BindGroupEntry<'_>> = Vec::new();
    for (pass, ((pass_plan, pass_gpu), offsets)) in
        plan.passes.iter().zip(passes_gpu).zip(&layout.iteration_offsets).enumerate()
    {
        input_key.clear();
        input_key.extend(pass_plan.inputs.iter().map(bound_input));
        if cache.inputs_stale(pass, &input_key) {
            input_entries.clear();
            for (input, slot) in pass_plan.inputs.iter().enumerate() {
                let (view, nearest) = match slot {
                    ResolvedSlot::Binding(binding) => {
                        let entry = &textures.entries[&dispatch.bindings[*binding as usize]];
                        let nearest = entry.sampling == TextureSampling::Nearest || !entry.format.filterable();
                        (cache.binding_view(*binding), nearest)
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
                input_entries.push(wgpu::BindGroupEntry {
                    binding: base + 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                });
            }
            let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("aether program inputs bind group"),
                layout: &pass_gpu.inputs_layout,
                entries: &input_entries,
            });
            cache.store_inputs(pass, &input_key, group);
        }

        let uniform_bind_group = cache.uniform_group(pass);
        let inputs_bind_group = cache.inputs_group(pass);
        let target_view = match pass_plan.output {
            ResolvedSlot::Binding(binding) => cache.binding_view(binding),
            ResolvedSlot::Transient(transient) => transient_view(transient),
        };
        // The pass's timestamp pair, claimed once for the whole repeat:
        // its first iteration opens the span and its last closes it, so
        // one bracket attributes every iteration to the pass entry.
        let bracket = queries
            .as_mut()
            .and_then(|queries| queries.open(dispatch.program_id, u32::try_from(pass).expect("pass index fits u32")));
        let iterations = offsets.len();
        for (iteration, &uniform_offset) in offsets.iter().enumerate() {
            let timestamps = bracket.and_then(|query| {
                queries.as_ref().and_then(|queries| queries.timestamps(query, iteration, iterations))
            });
            // The dispatch's first write to a slot clears it and every
            // later one loads it, so a chain accumulates; a repeat's
            // later iterations load for the same reason.
            let first_write = layout.clears_output[pass] && iteration == 0;
            let Some(draw) = &pass_plan.draw else {
                record_program_pass(
                    encoder,
                    &ProgramPassDraw {
                        pipeline: &pass_gpu.pipeline,
                        target_view,
                        clear: first_write,
                        uniform_bind_group,
                        uniform_offset,
                        inputs_bind_group,
                        timestamps,
                    },
                );
                continue;
            };

            // A draw pass's declared load is authoritative on its color
            // output; its later repeat iterations load so a repeat
            // accumulates rather than each iteration wiping the last.
            // Depth follows the shared-slot rule: the dispatch's first
            // reference to a slot clears it, later ones load it.
            let depth = draw.depth.map(|slot| ProgramDepthAttachment {
                view: depth_view(slot),
                clear: layout.clears_depth[pass] && iteration == 0,
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
                    uniform_bind_group,
                    uniform_offset,
                    inputs_bind_group,
                    vertex_buffer: &realized.vertex_buffer,
                    index_buffer: &realized.index_buffer,
                    index_count: realized.index_count,
                    timestamps,
                },
            );
        }
    }
}
