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
    ProgramDrawPipelineSpec, build_fullscreen_vertex_module, build_program_draw_pipeline, build_program_pipeline,
    program_inputs_layout, program_uniform_layout,
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
use validate::ProgramPlan;

/// Minimum bytes a pass binds for its uniform window: a zero-length
/// window (a uniform-less pass) still binds a 4-byte zeroed dummy so
/// every pipeline layout carries the same group-0 shape.
const MIN_BOUND_UNIFORM_BYTES: u64 = 4;

/// One registered program: the validated plan, the per-pass GPU
/// resources built at register time, and the per-dispatch setup the
/// executor derives once and reuses while the dispatch keeps binding the
/// same resources. The cache lives here rather than in the registry so
/// `program_destroy` releases it with the program it belongs to.
struct RegisteredProgram {
    plan: ProgramPlan,
    passes_gpu: Vec<PassGpu>,
    cache: DispatchCache,
    /// Per-pass GPU duration EWMAs (iamacoffeepot/aether#4423), held
    /// here for the same reason the cache is: they describe this
    /// program's graph, so `program_destroy` releases them with it and a
    /// re-register starts unmeasured under a fresh id.
    timings: PassCosts,
}

/// Per-pass GPU resources: the compiled pipeline and the layouts the
/// dispatch path builds its per-dispatch bind groups against.
struct PassGpu {
    pipeline: wgpu::RenderPipeline,
    uniform_layout: wgpu::BindGroupLayout,
    inputs_layout: wgpu::BindGroupLayout,
    bound_uniform_bytes: u64,
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
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let fullscreen = self.fullscreen_module.take().unwrap_or_else(|| build_fullscreen_vertex_module(device));
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aether program shader"),
            source: wgpu::ShaderSource::Wgsl(mail.wgsl.as_str().into()),
        });
        let passes_gpu: Vec<PassGpu> = plan
            .passes
            .iter()
            .map(|pass| {
                let bound_uniform_bytes = u64::from(pass.uniform_length).max(MIN_BOUND_UNIFORM_BYTES);
                let uniform_layout = program_uniform_layout(device, bound_uniform_bytes);
                let filterable: Vec<bool> =
                    pass.inputs.iter().map(|slot| plan.slot_format(*slot).filterable()).collect();
                let inputs_layout = program_inputs_layout(device, &filterable);
                let output_format = plan.slot_format(pass.output);
                let color_format = super::texture::wgpu_texture_format(output_format);
                let pipeline = match &pass.draw {
                    None => build_program_pipeline(
                        device,
                        &fullscreen,
                        &module,
                        &pass.entry_point,
                        color_format,
                        blend_for(output_format),
                        &uniform_layout,
                        &inputs_layout,
                    ),
                    Some(draw) => {
                        let layout = &plan.geometries[draw.geometry as usize].layout;
                        let attributes = wgpu_vertex_attributes(layout);
                        build_program_draw_pipeline(
                            device,
                            &ProgramDrawPipelineSpec {
                                module: &module,
                                vertex_entry_point: &draw.vertex_entry_point,
                                fragment_entry_point: &pass.entry_point,
                                vertex_stride_bytes: u64::try_from(vertex_stride_bytes(layout))
                                    .expect("vertex stride fits u64"),
                                vertex_attributes: &attributes,
                                color_format,
                                blend: blend_for(output_format),
                                depth: draw.depth.is_some(),
                                uniform_layout: &uniform_layout,
                                inputs_layout: &inputs_layout,
                            },
                        )
                    }
                };
                PassGpu { pipeline, uniform_layout, inputs_layout, bound_uniform_bytes }
            })
            .collect();
        self.fullscreen_module = Some(fullscreen);
        if let Some(error) = pollster::block_on(scope.pop()) {
            return ProgramRegisterResult::Err { reason: format!("pipeline creation failed: {error}") };
        }

        let program_id = self.next_id;
        self.next_id += 1;
        let cache = DispatchCache::new(&plan);
        let timings = PassCosts::new(&plan);
        self.entries.insert(program_id, RegisteredProgram { plan, passes_gpu, cache, timings });
        ProgramRegisterResult::Ok { program_id }
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

/// Blend state per output format: blendable color formats alpha-blend
/// over the target; a data plane replaces, which is what a pass writing
/// a quantity rather than a colour means by writing it.
fn blend_for(format: TextureFormat) -> Option<wgpu::BlendState> {
    match format {
        TextureFormat::Rgba8 | TextureFormat::R8 => Some(wgpu::BlendState::ALPHA_BLENDING),
        TextureFormat::R32Float | TextureFormat::R16Float => None,
    }
}
