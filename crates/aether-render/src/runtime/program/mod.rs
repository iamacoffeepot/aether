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
    build_fullscreen_vertex_module, build_program_pipeline, program_inputs_layout, program_uniform_layout,
};

use super::pipeline::RenderGpu;
use super::texture::TextureRegistry;
use crate::{ProgramDestroy, ProgramDispatch, ProgramRegister, ProgramRegisterResult, TextureFormat};

mod record;
mod validate;

use validate::ProgramPlan;

/// Minimum bytes a pass binds for its uniform window: a zero-length
/// window (a uniform-less pass) still binds a 4-byte zeroed dummy so
/// every pipeline layout carries the same group-0 shape.
const MIN_BOUND_UNIFORM_BYTES: u64 = 4;

/// One registered program: the validated plan plus the per-pass GPU
/// resources built at register time.
struct RegisteredProgram {
    plan: ProgramPlan,
    passes_gpu: Vec<PassGpu>,
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
}

impl ProgramRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self { next_id: 0, entries: HashMap::new(), fullscreen_module: None, transient_pool: HashMap::new() }
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
                let pipeline = build_program_pipeline(
                    device,
                    &fullscreen,
                    &module,
                    &pass.entry_point,
                    super::texture::wgpu_texture_format(output_format),
                    blend_for(output_format),
                    &uniform_layout,
                    &inputs_layout,
                );
                PassGpu { pipeline, uniform_layout, inputs_layout, bound_uniform_bytes }
            })
            .collect();
        self.fullscreen_module = Some(fullscreen);
        if let Some(error) = pollster::block_on(scope.pop()) {
            return ProgramRegisterResult::Err { reason: format!("pipeline creation failed: {error}") };
        }

        let program_id = self.next_id;
        self.next_id += 1;
        self.entries.insert(program_id, RegisteredProgram { plan, passes_gpu });
        ProgramRegisterResult::Ok { program_id }
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
        dispatches: &[ProgramDispatch],
    ) {
        for dispatch in dispatches {
            let Some(program) = self.entries.get(&dispatch.program_id) else {
                tracing::warn!(
                    target: "aether_render",
                    program_id = dispatch.program_id,
                    "program dispatch for unknown program id; dropping the dispatch",
                );
                continue;
            };
            record::record_dispatch(gpu, encoder, program, &mut self.transient_pool, textures, dispatch);
        }
    }
}

/// Blend state per output format: blendable color formats alpha-blend
/// over the target; `R32Float` cannot blend in core WebGPU, so its
/// passes replace.
fn blend_for(format: TextureFormat) -> Option<wgpu::BlendState> {
    match format {
        TextureFormat::Rgba8 | TextureFormat::R8 => Some(wgpu::BlendState::ALPHA_BLENDING),
        TextureFormat::R32Float => None,
    }
}
