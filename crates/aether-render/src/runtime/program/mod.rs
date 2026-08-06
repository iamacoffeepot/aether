//! The authored-render-program registry and executor (ADR-0170): the
//! session-scoped table of registered programs, the register path
//! (validation in [`validate`], pipeline construction under a wgpu
//! validation error scope here), and the dispatch record path
//! ([`record`]). The substrate side owns the stage primitives — the
//! fullscreen vertex module, layout shapes, pipeline builder, and pass
//! recorder — in `aether_substrate::render`; this module owns the policy
//! that decides what to build and record through them.

use std::collections::HashMap;

use aether_substrate::render::{
    ProgramComputePipelineSpec, ProgramDrawPipelineSpec, build_fullscreen_vertex_module,
    build_program_compute_pipeline, build_program_draw_pipeline, build_program_pipeline, program_inputs_layout,
    program_storage_layout, program_uniform_layout,
};

use super::geometry::{GeometryRegistry, wgpu_vertex_attributes};
use super::pipeline::RenderGpu;
use super::texture::TextureRegistry;
use crate::kinds::vertex_stride_bytes;
use crate::{
    ProgramDestroy, ProgramDispatch, ProgramRegister, ProgramRegisterResult, ProgramTimings, ProgramTimingsResult,
    TextureFormat,
};

mod cache;
mod record;
mod timing;
mod validate;

use cache::DispatchCache;
use timing::{Availability, PassCosts, PassTimingInstrument};
use validate::{PassPlanStage, ProgramPlan};

/// Minimum bytes a pass binds for its uniform window: a zero-length
/// window (a uniform-less pass) still binds a 4-byte zeroed dummy so
/// every pipeline layout carries the same group-0 shape.
const MIN_BOUND_UNIFORM_BYTES: u64 = 4;

/// One registered program: the durable authored source and validated
/// plan, the current device's ready-or-quarantined state, and folded
/// timing samples. A replacement-device compile failure quarantines this
/// entry without disturbing its durable state or any sibling program.
#[allow(dead_code, reason = "retained WGSL is consumed by the next recovery slice's runtime wiring")]
struct RegisteredProgram {
    plan: ProgramPlan,
    wgsl: String,
    state: ProgramDeviceState,
    /// Per-pass GPU duration EWMAs (iamacoffeepot/aether#4423), held
    /// here for the same reason the cache is: they describe this
    /// program's graph, so `program_destroy` releases them with it and a
    /// re-register starts unmeasured under a fresh id.
    timings: PassCosts,
}

/// Device-bound state for a registered program. Only replacement-device
/// compilation produces `Quarantined`: initial registration still
/// rejects before consuming an id, while a durable existing id survives
/// a replacement even when its shader no longer compiles there.
#[allow(dead_code, reason = "replacement quarantine is consumed by the next recovery slice's runtime wiring")]
enum ProgramDeviceState {
    Ready { passes_gpu: Vec<PassGpu>, cache: Box<DispatchCache> },
    Quarantined { reason: String },
}

/// Per-pass GPU resources: the compiled pipeline and the layouts the
/// dispatch path builds its per-dispatch bind groups against.
struct PassGpu {
    pipeline: PassPipeline,
    uniform_layout: wgpu::BindGroupLayout,
    inputs_layout: wgpu::BindGroupLayout,
    storage_layout: Option<wgpu::BindGroupLayout>,
    bound_uniform_bytes: u64,
}

enum PassPipeline {
    Render(wgpu::RenderPipeline),
    Compute(wgpu::ComputePipeline),
}

/// Pool key: resolved size plus realized format. Pooling on the
/// resolved values (not the declared `SlotExtent`) unifies slots that
/// resolve to the same texture — `Full` and `Divided { divisor: 1 }`
/// share allocations, and programs share the pool with each other.
type TransientKey = (u32, u32, wgpu::TextureFormat);

/// Session-scoped registry of authored render programs, plus the shared
/// transient pool their dispatches allocate intermediates from. Each
/// pooled entry is the transient texture's view — it serves as both the
/// pass attachment and the sampled input, and keeps the texture alive.
pub struct ProgramRegistry {
    next_id: u32,
    entries: HashMap<u32, RegisteredProgram>,
    /// The shared fullscreen vertex module, built on first register.
    fullscreen_module: Option<wgpu::ShaderModule>,
    /// Pooled transient intermediates keyed by resolved extent + format,
    /// persistent across dispatches so a repaint reuses its allocations.
    transient_pool: HashMap<TransientKey, Vec<wgpu::TextureView>>,
    /// Whether the operator wants per-pass GPU timings measured
    /// (iamacoffeepot/aether#4423). Resolved at boot; the instrument
    /// itself needs a device, so it is built on first use.
    timings_enabled: bool,
    /// The session's timestamp-query machinery, built against the device
    /// the first time a frame records — the registry outlives the lazy
    /// GPU boot, so it cannot be built in `new`.
    timings: Option<PassTimingInstrument>,
}

impl ProgramRegistry {
    /// A registry with the per-pass timing instrument enabled or
    /// disabled. Disabled brackets nothing and allocates no query
    /// machinery at all, and reports `Absent` with that as the reason.
    #[must_use]
    pub fn new(timings_enabled: bool) -> Self {
        Self {
            next_id: 0,
            entries: HashMap::new(),
            fullscreen_module: None,
            transient_pool: HashMap::new(),
            timings_enabled,
            timings: None,
        }
    }

    /// Register a program: validate (naga, then the graph), then build
    /// every pass pipeline under a wgpu validation error scope so a
    /// bad-but-naga-valid program — a sampler/layout mismatch, an
    /// input-count disagreement with the shader — replies `Err` instead
    /// of crashing the substrate. A rejected register consumes no id.
    pub fn register(&mut self, gpu: &RenderGpu, mail: ProgramRegister) -> ProgramRegisterResult {
        let plan = match validate::validate(&mail) {
            Ok(plan) => plan,
            Err(reason) => return ProgramRegisterResult::Err { reason },
        };

        let device = &gpu.device;
        let fullscreen = self.fullscreen_module.get_or_insert_with(|| build_fullscreen_vertex_module(device));
        let passes_gpu = match build_program_passes(device, fullscreen, &plan, &mail.wgsl) {
            Ok(passes_gpu) => passes_gpu,
            Err(reason) => return ProgramRegisterResult::Err { reason },
        };

        let program_id = self.next_id;
        self.next_id += 1;
        let cache = DispatchCache::new(&plan);
        let timings = PassCosts::new(&plan);
        self.entries.insert(
            program_id,
            RegisteredProgram {
                plan,
                wgsl: mail.wgsl,
                state: ProgramDeviceState::Ready { passes_gpu, cache: Box::new(cache) },
                timings,
            },
        );
        ProgramRegisterResult::Ok { program_id }
    }

    /// Rebuild all device-bound program state against a replacement
    /// device. Public ids, validated plans, authored WGSL, and folded
    /// timing samples survive. Shared modules, transient allocations,
    /// timing instrumentation, pipelines, and dispatch caches are
    /// discarded. Programs compile independently so one failure
    /// quarantines only that existing id.
    #[allow(dead_code, reason = "device-loss runtime wiring lands in the next recovery slice")]
    pub fn rebuild_for_device(&mut self, gpu: &RenderGpu) {
        self.fullscreen_module = None;
        self.transient_pool.clear();
        self.timings = None;

        if self.entries.is_empty() {
            return;
        }
        let fullscreen = build_fullscreen_vertex_module(&gpu.device);
        for (&program_id, program) in &mut self.entries {
            program.state = match build_program_passes(&gpu.device, &fullscreen, &program.plan, &program.wgsl) {
                Ok(passes_gpu) => {
                    ProgramDeviceState::Ready { cache: Box::new(DispatchCache::new(&program.plan)), passes_gpu }
                }
                Err(reason) => {
                    tracing::warn!(
                        target: "aether_render",
                        program_id,
                        %reason,
                        "program failed to compile on the replacement render device; quarantining it",
                    );
                    ProgramDeviceState::Quarantined { reason }
                }
            };
        }
        self.fullscreen_module = Some(fullscreen);
    }

    /// The per-pass GPU duration table one registered program has
    /// accumulated (iamacoffeepot/aether#4423). `Absent` when the
    /// instrument is not running — an adapter without `TIMESTAMP_QUERY`
    /// or an operator who turned it off — so a caller can tell "this
    /// device cannot answer" from "these passes cost nothing".
    pub fn timings(&self, mail: &ProgramTimings) -> ProgramTimingsResult {
        let reason = match self.timings.as_ref().map(PassTimingInstrument::availability) {
            Some(Availability::Running { .. }) => None,
            Some(Availability::Absent { reason }) => Some(reason.clone()),
            // No frame has recorded yet, so the instrument has not met a
            // device. Whether it will measure is not yet knowable, and
            // saying so beats guessing either way.
            None if self.timings_enabled => {
                Some("no frame has recorded yet, so the timing instrument has not met the render device".to_owned())
            }
            None => Some("per-pass gpu timings are disabled by configuration".to_owned()),
        };
        if let Some(reason) = reason {
            return ProgramTimingsResult::Absent { reason };
        }
        let Some(program) = self.entries.get(&mail.program_id) else {
            return ProgramTimingsResult::Err { reason: format!("unknown program id {}", mail.program_id) };
        };
        ProgramTimingsResult::Ok { program_id: mail.program_id, rows: program.timings.rows(&program.plan) }
    }

    /// Release a registered program, mirroring `destroy_texture`: an
    /// unknown id warn-drops. Pooled transients stay in the shared pool.
    pub fn destroy(&mut self, mail: &ProgramDestroy) {
        if self.entries.remove(&mail.program_id).is_none() {
            tracing::warn!(
                target: "aether_render",
                program_id = mail.program_id,
                "program destroy for unknown program id; dropping",
            );
        }
    }

    /// Execute the frame's pending dispatches into `encoder`, in arrival
    /// order. Called at the top of the frame record, before the world /
    /// material / overlay passes, so those passes sample freshly written
    /// program outputs in the same submission.
    pub fn record(
        &mut self,
        gpu: &RenderGpu,
        encoder: &mut wgpu::CommandEncoder,
        textures: &mut TextureRegistry,
        geometries: &mut GeometryRegistry,
        dispatches: &[ProgramDispatch],
    ) {
        // Fold whatever the device finished mapping since the last frame
        // before this frame claims a readback slot, then open the frame's
        // query budget against the passes the pending dispatches declare.
        let enabled = self.timings_enabled;
        let instrument =
            self.timings.get_or_insert_with(|| PassTimingInstrument::new(&gpu.device, &gpu.queue, enabled));
        instrument.harvest(&gpu.device, &mut self.entries);
        let declared: usize = dispatches
            .iter()
            .filter_map(|dispatch| self.entries.get(&dispatch.program_id))
            .filter(|program| matches!(program.state, ProgramDeviceState::Ready { .. }))
            .map(|program| program.plan.passes.len())
            .sum();
        let measuring = instrument.begin_frame(&gpu.device, declared);

        let Self { entries, transient_pool, timings, .. } = self;
        let instrument = timings.as_mut().expect("the instrument was inserted above");
        for dispatch in dispatches {
            let Some(program) = entries.get_mut(&dispatch.program_id) else {
                tracing::warn!(
                    target: "aether_render",
                    program_id = dispatch.program_id,
                    "program dispatch for unknown program id; dropping the dispatch",
                );
                continue;
            };
            let queries = measuring.then(|| instrument.frame()).flatten();
            record::record_dispatch(gpu, encoder, program, transient_pool, textures, geometries, dispatch, queries);
        }
        instrument.end_frame(encoder);
    }

    /// Request the map of the timing readback the frame just submitted.
    /// Separate from [`Self::record`] because a buffer still named by
    /// unsubmitted commands cannot be mapped.
    pub fn after_frame_submit(&mut self) {
        if let Some(instrument) = self.timings.as_mut() {
            instrument.after_submit();
        }
    }
}

/// Compile one program's complete per-pass GPU state under a scoped
/// validation error. Registration and replacement-device rebuilding use
/// this same construction path so their pipeline shapes cannot drift.
fn build_program_passes(
    device: &wgpu::Device,
    fullscreen: &wgpu::ShaderModule,
    plan: &ProgramPlan,
    wgsl: &str,
) -> Result<Vec<PassGpu>, String> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("aether program shader"),
        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    let passes_gpu = plan
        .passes
        .iter()
        .map(|pass| {
            let bound_uniform_bytes = u64::from(pass.uniform_length).max(MIN_BOUND_UNIFORM_BYTES);
            let visibility = match &pass.stage {
                PassPlanStage::Fragment => wgpu::ShaderStages::FRAGMENT,
                PassPlanStage::Draw(_) | PassPlanStage::DrawIndexedIndirect(_) => wgpu::ShaderStages::VERTEX_FRAGMENT,
                PassPlanStage::Compute(_) => wgpu::ShaderStages::COMPUTE,
            };
            let uniform_layout = program_uniform_layout(device, bound_uniform_bytes, visibility);
            let filterable: Vec<bool> = pass.inputs.iter().map(|slot| plan.slot_format(*slot).filterable()).collect();
            let inputs_layout = program_inputs_layout(device, &filterable, visibility);
            let mut storage_layout = None;
            let pipeline = match &pass.stage {
                PassPlanStage::Fragment => {
                    let output_format = plan.slot_format(pass.output.expect("fragment pass has an output"));
                    PassPipeline::Render(build_program_pipeline(
                        device,
                        fullscreen,
                        &module,
                        &pass.entry_point,
                        super::texture::wgpu_texture_format(output_format),
                        blend_for(output_format),
                        &uniform_layout,
                        &inputs_layout,
                    ))
                }
                PassPlanStage::Draw(draw) | PassPlanStage::DrawIndexedIndirect(draw) => {
                    let output_format = plan.slot_format(pass.output.expect("draw pass has an output"));
                    let layout = &plan.geometries[draw.geometry as usize].layout;
                    let attributes = wgpu_vertex_attributes(layout);
                    PassPipeline::Render(build_program_draw_pipeline(
                        device,
                        &ProgramDrawPipelineSpec {
                            module: &module,
                            vertex_entry_point: &draw.vertex_entry_point,
                            fragment_entry_point: &pass.entry_point,
                            vertex_stride_bytes: u64::try_from(vertex_stride_bytes(layout))
                                .expect("vertex stride fits u64"),
                            vertex_attributes: &attributes,
                            color_format: super::texture::wgpu_texture_format(output_format),
                            blend: blend_for(output_format),
                            depth: draw.depth.is_some(),
                            uniform_layout: &uniform_layout,
                            inputs_layout: &inputs_layout,
                        },
                    ))
                }
                PassPlanStage::Compute(compute) => {
                    let read_only: Vec<bool> =
                        compute.buffers.iter().map(|binding| binding.access == crate::StorageAccess::Read).collect();
                    let layout = program_storage_layout(device, &read_only);
                    let pipeline = build_program_compute_pipeline(
                        device,
                        &ProgramComputePipelineSpec {
                            module: &module,
                            entry_point: &pass.entry_point,
                            uniform_layout: &uniform_layout,
                            inputs_layout: &inputs_layout,
                            storage_layout: &layout,
                        },
                    );
                    storage_layout = Some(layout);
                    PassPipeline::Compute(pipeline)
                }
            };
            PassGpu { pipeline, uniform_layout, inputs_layout, storage_layout, bound_uniform_bytes }
        })
        .collect();
    pollster::block_on(scope.pop())
        .map_or_else(|| Ok(passes_gpu), |error| Err(format!("pipeline creation failed: {error}")))
}

/// Blend state per output format: blendable color formats alpha-blend
/// over the target; a data plane replaces, which is what a pass writing
/// a quantity rather than a colour means by writing it.
fn blend_for(format: TextureFormat) -> Option<wgpu::BlendState> {
    match format {
        TextureFormat::Rgba8 | TextureFormat::R8 => Some(wgpu::BlendState::ALPHA_BLENDING),
        TextureFormat::R32Float | TextureFormat::R16Float => None,
    }
}

#[cfg(test)]
mod rebuild_tests {
    use std::slice;

    use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;
    use aether_substrate::render::create_program_transient;

    use super::*;
    use crate::runtime::surface::boot_offscreen;
    use crate::{
        CreateTexture, CreateTextureResult, OutputSlot, PassStage, ProgramPass, SlotExtent, SlotSpec, TextureSampling,
        TextureUsage,
    };

    const SOLID_WGSL: &str = r"
@fragment
fn fs_solid() -> @location(0) vec4<f32> {
    return vec4<f32>(0.25, 0.5, 0.75, 1.0);
}
";

    fn boot_gpu() -> RenderGpu {
        let booted = boot_offscreen(None);
        RenderGpu::new(booted.device, booted.queue, booted.format, 4, 4, booted.polygon_mode, 4096)
    }

    fn solid_program() -> ProgramRegister {
        ProgramRegister {
            wgsl: SOLID_WGSL.to_owned(),
            bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
            transients: Vec::new(),
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![ProgramPass {
                stage: PassStage::Fragment,
                entry_point: "fs_solid".to_owned(),
                inputs: Vec::new(),
                output: OutputSlot::Binding { index: 0 },
                uniform_offset: 0,
                uniform_length: 0,
                repeat: None,
            }],
        }
    }

    fn register(registry: &mut ProgramRegistry, gpu: &RenderGpu) -> u32 {
        let ProgramRegisterResult::Ok { program_id } = registry.register(gpu, solid_program()) else {
            panic!("solid program registers");
        };
        program_id
    }

    fn writable_texture(textures: &mut TextureRegistry) -> u32 {
        let CreateTextureResult::Ok { texture_id } = textures.create(CreateTexture {
            width: 4,
            height: 4,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        }) else {
            panic!("writable texture create accepted");
        };
        texture_id
    }

    fn encoder(gpu: &RenderGpu) -> wgpu::CommandEncoder {
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("program rebuild test") })
    }

    #[test]
    fn replacement_rebuild_preserves_ids_and_isolates_quarantine() {
        if !has_wgpu_adapter() {
            return;
        }
        let old_gpu = boot_gpu();
        let mut registry = ProgramRegistry::new(false);
        let healthy_id = register(&mut registry, &old_gpu);
        let quarantined_id = register(&mut registry, &old_gpu);
        let mut textures = TextureRegistry::new();
        let healthy_texture_id = writable_texture(&mut textures);
        let quarantined_texture_id = writable_texture(&mut textures);
        let mut geometries = GeometryRegistry::new();

        // Warm the healthy program's old-device dispatch cache and output
        // realization. Reusing either on the replacement device is the
        // cross-device validation failure this test guards against.
        let mut old_encoder = encoder(&old_gpu);
        registry.record(
            &old_gpu,
            &mut old_encoder,
            &mut textures,
            &mut geometries,
            &[ProgramDispatch {
                program_id: healthy_id,
                bindings: vec![healthy_texture_id],
                geometries: Vec::new(),
                uniforms: Vec::new(),
            }],
        );
        assert!(textures.entries[&healthy_texture_id].realized.is_some());
        registry.transient_pool.insert(
            (1, 1, wgpu::TextureFormat::Rgba8Unorm),
            vec![
                create_program_transient(&old_gpu.device, 1, 1, wgpu::TextureFormat::Rgba8Unorm)
                    .create_view(&wgpu::TextureViewDescriptor::default()),
            ],
        );
        assert!(registry.timings.is_some(), "recording initializes the old-device timing instrument");

        registry.entries.get_mut(&quarantined_id).expect("registered program").wgsl = "not valid wgsl".to_owned();
        let replacement_gpu = boot_gpu();
        textures.invalidate_device_resources();
        registry.rebuild_for_device(&replacement_gpu);

        assert_eq!(registry.next_id, 2, "replacement must not rewind public ids");
        assert!(registry.entries.contains_key(&healthy_id));
        assert!(registry.entries.contains_key(&quarantined_id));
        assert_eq!(registry.entries[&healthy_id].wgsl, SOLID_WGSL);
        assert!(matches!(registry.entries[&healthy_id].state, ProgramDeviceState::Ready { .. }));
        let ProgramDeviceState::Quarantined { reason } = &registry.entries[&quarantined_id].state else {
            panic!("only the replacement-incompatible program is quarantined");
        };
        assert!(reason.contains("pipeline creation failed"), "quarantine retains the scoped compiler reason: {reason}");
        assert!(registry.transient_pool.is_empty(), "old-device transient views must be discarded");
        assert!(registry.timings.is_none(), "old-device query pools and readbacks must be discarded");
        assert!(registry.fullscreen_module.is_some(), "the shared module is rebuilt on the replacement device");

        let mut replacement_encoder = encoder(&replacement_gpu);
        registry.record(
            &replacement_gpu,
            &mut replacement_encoder,
            &mut textures,
            &mut geometries,
            &[
                ProgramDispatch {
                    program_id: healthy_id,
                    bindings: vec![healthy_texture_id],
                    geometries: Vec::new(),
                    uniforms: Vec::new(),
                },
                ProgramDispatch {
                    program_id: quarantined_id,
                    bindings: vec![quarantined_texture_id],
                    geometries: Vec::new(),
                    uniforms: Vec::new(),
                },
            ],
        );
        assert!(
            textures.entries[&healthy_texture_id].realized.is_some(),
            "the healthy program records through freshly built replacement-device state",
        );
        assert!(
            textures.entries[&quarantined_texture_id].realized.is_none(),
            "a quarantined dispatch drops before it realizes any binding",
        );

        let next_id = register(&mut registry, &replacement_gpu);
        assert_eq!(next_id, 2, "new registration continues after the preserved id sequence");
    }

    #[test]
    fn replacement_rebuild_preserves_folded_timing_samples() {
        if !has_wgpu_adapter() {
            return;
        }
        let old_gpu = boot_gpu();
        if !old_gpu.device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return;
        }
        let mut registry = ProgramRegistry::new(true);
        let program_id = register(&mut registry, &old_gpu);
        let mut textures = TextureRegistry::new();
        let texture_id = writable_texture(&mut textures);
        let mut geometries = GeometryRegistry::new();
        let dispatch =
            ProgramDispatch { program_id, bindings: vec![texture_id], geometries: Vec::new(), uniforms: Vec::new() };

        for _ in 0..3 {
            let mut frame_encoder = encoder(&old_gpu);
            registry.record(&old_gpu, &mut frame_encoder, &mut textures, &mut geometries, slice::from_ref(&dispatch));
            old_gpu.queue.submit([frame_encoder.finish()]);
            registry.after_frame_submit();
            old_gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("timed frame completes");

            let mut harvest_encoder = encoder(&old_gpu);
            registry.record(&old_gpu, &mut harvest_encoder, &mut textures, &mut geometries, &[]);
            if registry.entries[&program_id].timings.rows(&registry.entries[&program_id].plan)[0].samples > 0 {
                break;
            }
        }
        let before = registry.entries[&program_id].timings.rows(&registry.entries[&program_id].plan);
        assert!(before[0].samples > 0, "the pre-replacement device must fold at least one timing sample");

        let replacement_gpu = boot_gpu();
        registry.rebuild_for_device(&replacement_gpu);

        let after = registry.entries[&program_id].timings.rows(&registry.entries[&program_id].plan);
        assert_eq!(after[0].samples, before[0].samples);
        assert_eq!(after[0].mean_nanos, before[0].mean_nanos);
        assert_eq!(after[0].mad_nanos, before[0].mad_nanos);
        assert_eq!((after[0].width, after[0].height), (before[0].width, before[0].height));
        assert!(registry.timings.is_none(), "only the old device's instrument is reset, not folded costs");
    }
}
