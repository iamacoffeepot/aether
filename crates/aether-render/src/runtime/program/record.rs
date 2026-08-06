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
//! every frame — see `super::cache` for what invalidates what. GPU errors
//! raised after those checks are scoped to setup or to the pass being
//! recorded, before the dispatch address is lost to the device handler.

use std::collections::HashMap;

use aether_substrate::render::{
    PROGRAM_DEPTH_FORMAT, ProgramComputePass, ProgramDepthAttachment, ProgramDrawCommand, ProgramDrawPass,
    ProgramPassDraw, create_program_depth_transient, create_program_transient, record_program_compute_pass,
    record_program_draw_pass, record_program_pass,
};

use super::super::geometry::{GeometryRegistry, INDIRECT_CONTROL_BYTES};
use super::super::pipeline::RenderGpu;
use super::super::surface::render_limits;
use super::super::texture::TextureRegistry;
use super::cache::{BoundInput, BoundStorage, CacheParts};
use super::timing::FrameQueries;
use super::validate::{PassPlan, PassPlanStage, ProgramPlan, ResolvedSlot, resolve_extent};
use super::{PassGpu, PassPipeline, ProgramDeviceState, RegisteredProgram, TransientKey};
use crate::{GeometryBuffer, PassLoad, ProgramDispatch, TextureSampling, TextureUsage};

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
    let RegisteredProgram { plan, state, timings, .. } = program;
    let ProgramDeviceState::Ready { passes_gpu, cache } = state else {
        let ProgramDeviceState::Quarantined { reason } = state else {
            unreachable!("the ready state returned above")
        };
        tracing::warn!(
            target: "aether_render",
            program_id = dispatch.program_id,
            %reason,
            "program dispatch targets a replacement-device-quarantined program; dropping the dispatch",
        );
        return;
    };
    let Some(reference) = check_dispatch(plan, textures, geometries, dispatch) else {
        return;
    };
    timings.observe_reference(reference);

    // Dispatch-time wgpu work used to escape only through the device's
    // uncaptured handler, after its program/pass address had been lost
    // (iamacoffeepot/aether#4426). On the native wgpu-core backend used by
    // the render runtime, popping these scopes is a thread-local CPU-stack
    // operation whose future is already ready; it does not poll or wait for
    // the device. The ignored cost instrument below keeps that assumption
    // measurable on a real adapter.
    let setup_scopes = GpuErrorScopes::push(&gpu.device);

    for &texture_id in &dispatch.bindings {
        if let Some(entry) = textures.entries.get_mut(&texture_id) {
            entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
        }
    }
    for pass_plan in &plan.passes {
        if let Some(draw) = pass_plan.stage.draw()
            && let Some(entry) = geometries.entries.get_mut(&dispatch.geometries[draw.geometry as usize])
        {
            entry.ensure_realized(&gpu.device, &gpu.queue);
        }
        if let Some(compute) = pass_plan.stage.compute() {
            for binding in &compute.buffers {
                if let Some(entry) = geometries.entries.get_mut(&dispatch.geometries[binding.geometry as usize]) {
                    entry.ensure_realized(&gpu.device, &gpu.queue);
                }
            }
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

    parts.upload_uniforms(&gpu.device, &gpu.queue, plan, passes_gpu, &dispatch.uniforms);
    if report_setup_gpu_errors(setup_scopes.pop(), dispatch) {
        return;
    }

    encode_passes(gpu, encoder, plan, passes_gpu, &mut parts, pool, textures, geometries, dispatch, queries);
}

/// The three WebGPU error classes a dispatch can produce. One nested scope
/// per filter keeps every class out of the generic uncaptured handler while
/// the work still has its program/pass address. Scopes must pop in reverse
/// order; the field order below mirrors the explicit pop order.
struct GpuErrorScopes {
    validation: wgpu::ErrorScopeGuard,
    internal: wgpu::ErrorScopeGuard,
    out_of_memory: wgpu::ErrorScopeGuard,
}

impl GpuErrorScopes {
    fn push(device: &wgpu::Device) -> Self {
        let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        Self { validation, internal, out_of_memory }
    }

    fn pop(self) -> GpuErrors {
        let validation = pollster::block_on(self.validation.pop());
        let internal = pollster::block_on(self.internal.pop());
        let out_of_memory = pollster::block_on(self.out_of_memory.pop());
        GpuErrors { validation, internal, out_of_memory }
    }
}

struct GpuErrors {
    validation: Option<wgpu::Error>,
    internal: Option<wgpu::Error>,
    out_of_memory: Option<wgpu::Error>,
}

impl GpuErrors {
    fn into_classified(self) -> impl Iterator<Item = (&'static str, wgpu::Error)> {
        [("validation", self.validation), ("internal", self.internal), ("out_of_memory", self.out_of_memory)]
            .into_iter()
            .filter_map(|(class, error)| error.map(|error| (class, error)))
    }
}

fn report_setup_gpu_errors(errors: GpuErrors, dispatch: &ProgramDispatch) -> bool {
    let mut failed = false;
    for (error_class, error) in errors.into_classified() {
        failed = true;
        tracing::error!(
            target: "aether_render",
            program_id = dispatch.program_id,
            phase = "setup",
            bindings = ?dispatch.bindings,
            geometries = ?dispatch.geometries,
            error_class,
            %error,
            "program dispatch gpu setup failed; dropping its pass recording",
        );
    }
    failed
}

fn report_pass_gpu_errors(errors: GpuErrors, dispatch: &ProgramDispatch, pass: usize, pass_plan: &PassPlan) -> bool {
    let mut failed = false;
    for (error_class, error) in errors.into_classified() {
        failed = true;
        tracing::error!(
            target: "aether_render",
            program_id = dispatch.program_id,
            pass,
            entry_point = %pass_plan.entry_point,
            input_slots = ?pass_plan.inputs,
            output_slot = ?pass_plan.output,
            stage = ?pass_plan.stage,
            bindings = ?dispatch.bindings,
            geometries = ?dispatch.geometries,
            error_class,
            %error,
            "program dispatch gpu pass failed; dropping its remaining pass recording",
        );
    }
    failed
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
        let mut geometry_bindings = Vec::new();
        if let Some(draw) = pass_plan.stage.draw() {
            geometry_bindings.push(draw.geometry);
        }
        if let Some(compute) = pass_plan.stage.compute() {
            geometry_bindings.extend(compute.buffers.iter().map(|binding| binding.geometry));
        }
        geometry_bindings.sort_unstable();
        geometry_bindings.dedup();
        for binding in geometry_bindings {
            let binding = binding as usize;
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

        if let Some(compute) = pass_plan.stage.compute() {
            let max_bytes = render_limits().max_storage_buffer_binding_size;
            for (storage_binding, declared) in compute.buffers.iter().enumerate() {
                let geometry_id = dispatch.geometries[declared.geometry as usize];
                let entry = &geometries.entries[&geometry_id];
                let bytes = match declared.buffer {
                    GeometryBuffer::Vertices => entry.vertices.len() as u64,
                    GeometryBuffer::Indices => entry.indices.len() as u64,
                    GeometryBuffer::DrawIndexedIndirect => INDIRECT_CONTROL_BYTES as u64,
                };
                if bytes == 0 {
                    tracing::warn!(
                        target: "aether_render",
                        program_id,
                        pass,
                        storage_binding,
                        geometry_id,
                        buffer = ?declared.buffer,
                        "program dispatch compute binding names an empty geometry buffer; dropping the dispatch",
                    );
                    return None;
                }
                if bytes > max_bytes {
                    tracing::warn!(
                        target: "aether_render",
                        program_id,
                        pass,
                        storage_binding,
                        geometry_id,
                        buffer = ?declared.buffer,
                        bytes,
                        max_bytes,
                        "program dispatch compute binding exceeds the storage binding size limit; dropping the dispatch",
                    );
                    return None;
                }
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
        if let Some(ResolvedSlot::Binding(output)) = pass_plan.output {
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
    let mut storage_key: Vec<BoundStorage> = Vec::new();
    let mut storage_entries: Vec<wgpu::BindGroupEntry<'_>> = Vec::new();
    for (pass, ((pass_plan, pass_gpu), offsets)) in
        plan.passes.iter().zip(passes_gpu).zip(&layout.iteration_offsets).enumerate()
    {
        let pass_scopes = GpuErrorScopes::push(&gpu.device);
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

        if let Some(compute) = pass_plan.stage.compute() {
            storage_key.clear();
            storage_entries.clear();
            for (binding, declared) in compute.buffers.iter().enumerate() {
                let geometry_id = dispatch.geometries[declared.geometry as usize];
                let entry = &geometries.entries[&geometry_id];
                let realized = entry.realized.as_ref().expect("realized before encode");
                let buffer = match declared.buffer {
                    GeometryBuffer::Vertices => &realized.vertex_buffer,
                    GeometryBuffer::Indices => &realized.index_buffer,
                    GeometryBuffer::DrawIndexedIndirect => &realized.indirect_buffer,
                };
                storage_key.push(BoundStorage {
                    geometry_id,
                    revision: entry.revision,
                    buffer: declared.buffer,
                    access: declared.access,
                });
                storage_entries.push(wgpu::BindGroupEntry {
                    binding: u32::try_from(binding).expect("program storage binding index fits u32"),
                    resource: buffer.as_entire_binding(),
                });
            }
            if cache.storage_stale(pass, &storage_key) {
                let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("aether program storage bind group"),
                    layout: pass_gpu.storage_layout.as_ref().expect("compute pass has a storage layout"),
                    entries: &storage_entries,
                });
                cache.store_storage(pass, &storage_key, group);
            }
        }

        let uniform_bind_group = cache.uniform_group(pass);
        let inputs_bind_group = cache.inputs_group(pass);
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
            match (&pass_plan.stage, &pass_gpu.pipeline) {
                (PassPlanStage::Fragment, PassPipeline::Render(pipeline)) => {
                    let target_view = match pass_plan.output.expect("fragment pass has an output") {
                        ResolvedSlot::Binding(binding) => cache.binding_view(binding),
                        ResolvedSlot::Transient(transient) => transient_view(transient),
                    };
                    record_program_pass(
                        encoder,
                        &ProgramPassDraw {
                            pipeline,
                            target_view,
                            clear: layout.clears_output[pass] && iteration == 0,
                            uniform_bind_group,
                            uniform_offset,
                            inputs_bind_group,
                            timestamps,
                        },
                    );
                }
                (
                    PassPlanStage::Draw(draw) | PassPlanStage::DrawIndexedIndirect(draw),
                    PassPipeline::Render(pipeline),
                ) => {
                    let target_view = match pass_plan.output.expect("draw pass has an output") {
                        ResolvedSlot::Binding(binding) => cache.binding_view(binding),
                        ResolvedSlot::Transient(transient) => transient_view(transient),
                    };
                    let depth = draw.depth.map(|slot| ProgramDepthAttachment {
                        view: depth_view(slot),
                        clear: layout.clears_depth[pass] && iteration == 0,
                    });
                    let realized = geometries.entries[&dispatch.geometries[draw.geometry as usize]]
                        .realized
                        .as_ref()
                        .expect("realized before encode");
                    let command = if matches!(&pass_plan.stage, PassPlanStage::DrawIndexedIndirect(_)) {
                        ProgramDrawCommand::Indirect { buffer: &realized.indirect_buffer }
                    } else {
                        ProgramDrawCommand::Direct { index_count: realized.index_count }
                    };
                    record_program_draw_pass(
                        encoder,
                        &ProgramDrawPass {
                            pipeline,
                            target_view,
                            clear_color: draw.load == PassLoad::Clear && iteration == 0,
                            depth,
                            uniform_bind_group,
                            uniform_offset,
                            inputs_bind_group,
                            vertex_buffer: &realized.vertex_buffer,
                            index_buffer: &realized.index_buffer,
                            command,
                            timestamps,
                        },
                    );
                }
                (PassPlanStage::Compute(compute), PassPipeline::Compute(pipeline)) => {
                    record_program_compute_pass(
                        encoder,
                        &ProgramComputePass {
                            pipeline,
                            uniform_bind_group,
                            uniform_offset,
                            inputs_bind_group,
                            storage_bind_group: cache.storage_group(pass),
                            workgroups: compute.workgroups,
                            timestamps,
                        },
                    );
                }
                _ => unreachable!("validated pass stage and built pipeline stay paired"),
            }
        }
        if report_pass_gpu_errors(pass_scopes.pop(), dispatch, pass, pass_plan) {
            return;
        }
    }
}

#[cfg(test)]
mod gpu_error_scope_tests {
    use std::time::Instant;

    use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;

    use super::*;
    use crate::runtime::surface::boot_offscreen;

    #[test]
    fn nested_scopes_capture_a_dispatch_validation_error_by_class() {
        if !has_wgpu_adapter() {
            return;
        }
        let gpu = boot_offscreen(None);
        let scopes = GpuErrorScopes::push(&gpu.device);
        let _invalid = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("deliberately oversized dispatch resource"),
            size: gpu.device.limits().max_buffer_size.checked_add(1).expect("buffer limit leaves room for test error"),
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });

        let errors = scopes.pop();
        assert!(errors.validation.is_some(), "the invalid resource must be caught as validation");
        assert!(errors.internal.is_none(), "a validation error must not be misclassified as internal");
        assert!(errors.out_of_memory.is_none(), "a validation error must not be misclassified as out-of-memory");
    }

    /// Manual cost instrument for issue #4426. It measures only the three
    /// empty push/pop pairs the production path adds per setup/pass; no GPU
    /// work is submitted, so the number isolates CPU bookkeeping.
    #[test]
    #[ignore = "instrument; reports native wgpu error-scope overhead"]
    #[allow(clippy::print_stderr)]
    fn empty_error_scope_cost() {
        const SAMPLES: u32 = 100_000;

        if !has_wgpu_adapter() {
            return;
        }
        let gpu = boot_offscreen(None);
        let started = Instant::now();
        for _ in 0..SAMPLES {
            let errors = GpuErrorScopes::push(&gpu.device).pop();
            assert!(errors.into_classified().next().is_none());
        }
        let nanos = started.elapsed().as_nanos() / u128::from(SAMPLES);
        eprintln!("three empty dispatch error scopes: {nanos} ns per push/pop set over {SAMPLES} samples");
    }
}
