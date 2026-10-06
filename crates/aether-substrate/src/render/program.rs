//! Authored-render-program pass primitives (ADR-0170). The substrate owns
//! the stage plumbing — a fullscreen-triangle vertex module, the uniform /
//! input bind group layout shapes, the per-pass pipeline builder, and the
//! pass recorder — while the render cap owns the program registry, the
//! graph validation, and the dispatch resolution that decide what to build
//! and record through these primitives. Mirrors the `quad` / `material`
//! split: low-level wgpu construction here, policy in `aether-render`.

/// The substrate-owned vertex stage every program pass shares: one
/// fullscreen triangle emitting `@location(0) uv` in texture convention
/// ((0, 0) top-left). Authored modules declare fragment entry points only.
pub const PROGRAM_FULLSCREEN_WGSL: &str = include_str!("program.wgsl");

/// Entry point name of the shared fullscreen vertex stage.
pub const PROGRAM_FULLSCREEN_ENTRY: &str = "vs_fullscreen";

/// Build the shared fullscreen vertex module. Built once per device and
/// reused by every program pipeline.
#[must_use]
pub fn build_fullscreen_vertex_module(device: &wgpu::Device) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("aether program fullscreen vertex shader"),
        source: wgpu::ShaderSource::Wgsl(PROGRAM_FULLSCREEN_WGSL.into()),
    })
}

/// Group-0 layout for a pass's uniform window: one uniform buffer with
/// a dynamic offset, so a repeated pass rebinds its
/// per-iteration window as an offset rather than a fresh bind group.
/// `bound_bytes` is the window length the pass binds (at least the
/// shader's declared block size — the render cap validates that at
/// register time). Visible to both stages: a draw pass's authored
/// vertex stage reads the same window its fragment stage does
/// (ADR-0171), and visibility a stage does not use costs nothing.
#[must_use]
pub fn program_uniform_layout(
    device: &wgpu::Device,
    bound_bytes: u64,
    visibility: wgpu::ShaderStages,
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("aether program uniform bind group layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(bound_bytes),
            },
            count: None,
        }],
    })
}

/// How the color texture at one program input is viewed, which is also
/// the type the shader declares for it.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProgramInputView {
    /// `texture_2d<f32>`.
    Plain,
    /// `texture_2d_array<f32>`.
    Array,
    /// `texture_3d<f32>`.
    Volume,
}

/// The sampler that accompanies one program input's color texture.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProgramInputSampler {
    /// A filtering sampler over a filterable texture.
    Filtering,
    /// A non-filtering sampler, for a format core WebGPU cannot
    /// linear-filter (`R32Float`).
    NonFiltering,
    /// No sampler: the shader reads the texture with `textureLoad`.
    None,
}

/// What accompanies a depth texture at one program input: a depth
/// texture is compared or loaded, and never read through a plain
/// sampler.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProgramDepthSampler {
    /// A `sampler_comparison`, read with `textureSampleCompare` or
    /// `textureSampleCompareLevel`.
    Comparison,
    /// No sampler: the shader reads the stored depth with `textureLoad`.
    None,
}

/// One input of a program pass, as its group-1 layout needs it: a color
/// texture with the sampler that reads it, or a depth texture with the
/// one a depth texture takes. The two are separate forms so that a
/// depth texture under a filtering sampler, which wgpu refuses only
/// once a pipeline is built against the layout, cannot be described.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProgramInput {
    /// `texture_2d<f32>`, `texture_2d_array<f32>` or `texture_3d<f32>`.
    Color { view: ProgramInputView, sampler: ProgramInputSampler },
    /// `texture_depth_2d`.
    Depth { sampler: ProgramDepthSampler },
}

impl ProgramInput {
    /// The texture entry's sample type and view dimension. A color
    /// entry is filterable exactly when its sampler is
    /// [`ProgramInputSampler::Filtering`]. Declaring the other two
    /// unfilterable is what lets a texture of any format bind there: a
    /// filterable texture satisfies an unfilterable entry, and the
    /// reverse does not hold.
    fn texture(self) -> (wgpu::TextureSampleType, wgpu::TextureViewDimension) {
        match self {
            Self::Color { view, sampler } => {
                let filterable = sampler == ProgramInputSampler::Filtering;
                let view_dimension = match view {
                    ProgramInputView::Plain => wgpu::TextureViewDimension::D2,
                    ProgramInputView::Array => wgpu::TextureViewDimension::D2Array,
                    ProgramInputView::Volume => wgpu::TextureViewDimension::D3,
                };
                (wgpu::TextureSampleType::Float { filterable }, view_dimension)
            }
            Self::Depth { .. } => (wgpu::TextureSampleType::Depth, wgpu::TextureViewDimension::D2),
        }
    }

    /// The sampler entry's binding type, or `None` for an input the
    /// shader reads with `textureLoad`.
    fn sampler(self) -> Option<wgpu::SamplerBindingType> {
        match self {
            Self::Color { sampler: ProgramInputSampler::Filtering, .. } => Some(wgpu::SamplerBindingType::Filtering),
            Self::Color { sampler: ProgramInputSampler::NonFiltering, .. } => {
                Some(wgpu::SamplerBindingType::NonFiltering)
            }
            Self::Depth { sampler: ProgramDepthSampler::Comparison } => Some(wgpu::SamplerBindingType::Comparison),
            Self::Color { sampler: ProgramInputSampler::None, .. }
            | Self::Depth { sampler: ProgramDepthSampler::None } => None,
        }
    }
}

/// Group-1 layout for a pass's inputs, in slot order. Input `n` is the
/// texture at `@binding(2n)` and, when it has one, the sampler at
/// `@binding(2n + 1)`. An input without a sampler leaves `2n + 1` out
/// of the layout, so the inputs after it keep their binding numbers
/// whatever the inputs before them declare.
///
/// Visible to every stage the caller names, for the same reason the
/// uniform window is (ADR-0172): a draw pass whose vertex stage
/// displaces geometry by a data plane — the ink ribbons reading their
/// own visibility field — must `textureLoad` that plane before the
/// rasterizer exists to have a fragment stage. Sampling with implicit
/// derivatives stays a fragment-only operation by WGSL's own rule, so
/// widening the layout grants a vertex stage `textureLoad` /
/// `textureSampleLevel` and nothing more, and visibility a stage does
/// not use costs nothing.
///
/// # Panics
/// Panics if the input count exceeds what a `u32` binding index holds,
/// unreachable behind WebGPU's per-stage sampled-texture limit.
#[must_use]
pub fn program_inputs_layout(
    device: &wgpu::Device,
    inputs: &[ProgramInput],
    visibility: wgpu::ShaderStages,
) -> wgpu::BindGroupLayout {
    let mut entries = Vec::with_capacity(inputs.len() * 2);
    for (input, description) in inputs.iter().enumerate() {
        let base = u32::try_from(input * 2).expect("program input binding index fits u32");
        let (sample_type, view_dimension) = description.texture();
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: base,
            visibility,
            ty: wgpu::BindingType::Texture { sample_type, view_dimension, multisampled: false },
            count: None,
        });

        let Some(sampler) = description.sampler() else {
            continue;
        };
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: base + 1,
            visibility,
            ty: wgpu::BindingType::Sampler(sampler),
            count: None,
        });
    }
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("aether program inputs bind group layout"),
        entries: &entries,
    })
}

/// Group-2 layout for a compute pass's resident geometry buffers. One
/// bool per binding states whether WGSL declared it read-only; every
/// binding is a whole storage buffer and is visible only to compute.
///
/// # Panics
/// Panics if the binding count exceeds `u32`, unreachable behind
/// WebGPU's per-stage storage-buffer limit.
#[must_use]
pub fn program_storage_layout(device: &wgpu::Device, read_only: &[bool]) -> wgpu::BindGroupLayout {
    let entries: Vec<_> = read_only
        .iter()
        .enumerate()
        .map(|(binding, &read_only)| wgpu::BindGroupLayoutEntry {
            binding: u32::try_from(binding).expect("program storage binding index fits u32"),
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        })
        .collect();
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("aether program storage bind group layout"),
        entries: &entries,
    })
}

/// One program pass's pipeline shape: the shared fullscreen vertex stage
/// (`vertex_module`) over the authored module's fragment `entry_point`,
/// rendering into a color attachment of `color_format` that holds
/// `sample_count` samples per texel. `blend` is the state the pass
/// declared: `None` replaces the attachment, which every format
/// allows, and `Some` is refused by pipeline creation on a format the
/// device cannot blend.
pub struct ProgramPipelineSpec<'a> {
    pub vertex_module: &'a wgpu::ShaderModule,
    pub fragment_module: &'a wgpu::ShaderModule,
    pub entry_point: &'a str,
    pub color_format: wgpu::TextureFormat,
    pub sample_count: u32,
    pub blend: Option<wgpu::BlendState>,
    pub uniform_layout: &'a wgpu::BindGroupLayout,
    pub inputs_layout: &'a wgpu::BindGroupLayout,
}

/// Build one program pass pipeline from its [`ProgramPipelineSpec`]. No
/// vertex buffers, no depth: program passes are pure image work.
#[must_use]
pub fn build_program_pipeline(device: &wgpu::Device, spec: &ProgramPipelineSpec<'_>) -> wgpu::RenderPipeline {
    let &ProgramPipelineSpec {
        vertex_module,
        fragment_module,
        entry_point,
        color_format,
        sample_count,
        blend,
        uniform_layout,
        inputs_layout,
    } = spec;
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether program pipeline layout"),
        bind_group_layouts: &[Some(uniform_layout), Some(inputs_layout)],
        immediate_size: 0,
    });
    let fragment_targets =
        [Some(wgpu::ColorTargetState { format: color_format, blend, write_mask: wgpu::ColorWrites::ALL })];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("aether program pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: vertex_module,
            entry_point: Some(PROGRAM_FULLSCREEN_ENTRY),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: fragment_module,
            entry_point: Some(entry_point),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &fragment_targets,
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: None,
        multisample: multisample(sample_count),
        multiview_mask: None,
        cache: None,
    })
}

/// The one depth format a draw pass's depth transient realizes as
/// (ADR-0171). Fixed rather than declared: a program's depth slot
/// carries an extent alone.
pub const PROGRAM_DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// One vertex buffer a draw pipeline reads: its stride, whether it
/// steps per vertex or per instance, and the attributes laid out in it.
/// The attributes come from a layout the render cap has already checked
/// the vertex stage's interface against.
pub struct ProgramVertexBuffer<'a> {
    pub stride_bytes: u64,
    pub step_mode: wgpu::VertexStepMode,
    pub attributes: &'a [wgpu::VertexAttribute],
}

/// The depth state of a draw pipeline that attaches a depth transient:
/// the test is always `LessEqual`, and `write` says whether a fragment
/// that passes it is written.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ProgramDepthState {
    pub write: bool,
}

/// The color target of a draw pipeline: the attachment's `format`, and
/// the `blend` state the pass declared, as on [`ProgramPipelineSpec`].
/// The two travel together because a pipeline with no color target has
/// neither.
#[derive(Debug, Copy, Clone)]
pub struct ProgramColorTarget {
    pub format: wgpu::TextureFormat,
    pub blend: Option<wgpu::BlendState>,
}

/// One draw pass's pipeline shape (ADR-0171, ADR-0246): the authored
/// module's vertex and fragment entry points over `vertex_buffers`, in
/// buffer-slot order, at `sample_count` samples per texel. A pass over
/// one bound geometry has one per-vertex buffer; a draw-sets pass adds
/// a per-instance buffer at slot 1. `cull_mode` is `None` for a pass
/// that draws both windings. `color` is the color attachment the pass
/// renders into, or `None` for a depth-only pass, whose pipeline has a
/// fragment stage and no color target. `depth` is the state a declared
/// depth transient attaches under, at the same sample count; a pass
/// declaring none rasterizes in draw order. A pipeline declares at
/// least one of the two.
pub struct ProgramDrawPipelineSpec<'a> {
    pub module: &'a wgpu::ShaderModule,
    pub vertex_entry_point: &'a str,
    pub fragment_entry_point: &'a str,
    pub vertex_buffers: &'a [ProgramVertexBuffer<'a>],
    pub cull_mode: Option<wgpu::Face>,
    pub color: Option<ProgramColorTarget>,
    pub sample_count: u32,
    pub depth: Option<ProgramDepthState>,
    pub uniform_layout: &'a wgpu::BindGroupLayout,
    pub inputs_layout: &'a wgpu::BindGroupLayout,
}

/// Build one draw pass pipeline from its [`ProgramDrawPipelineSpec`].
/// The front face is counter-clockwise; whether the other side is
/// culled is the pass's declaration, since the substrate has no view on
/// which side of a face it is painting.
#[must_use]
pub fn build_program_draw_pipeline(device: &wgpu::Device, spec: &ProgramDrawPipelineSpec<'_>) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether program draw pipeline layout"),
        bind_group_layouts: &[Some(spec.uniform_layout), Some(spec.inputs_layout)],
        immediate_size: 0,
    });
    let vertex_buffers: Vec<Option<wgpu::VertexBufferLayout<'_>>> = spec
        .vertex_buffers
        .iter()
        .map(|buffer| {
            Some(wgpu::VertexBufferLayout {
                array_stride: buffer.stride_bytes,
                step_mode: buffer.step_mode,
                attributes: buffer.attributes,
            })
        })
        .collect();
    // An `Option` as a slice is the target list itself: one target, or
    // none for a depth-only pipeline.
    let fragment_target = spec.color.map(|color| {
        Some(wgpu::ColorTargetState { format: color.format, blend: color.blend, write_mask: wgpu::ColorWrites::ALL })
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("aether program draw pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: spec.module,
            entry_point: Some(spec.vertex_entry_point),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &vertex_buffers,
        },
        fragment: Some(wgpu::FragmentState {
            module: spec.module,
            entry_point: Some(spec.fragment_entry_point),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: fragment_target.as_slice(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: spec.cull_mode,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: spec.depth.map(|depth| wgpu::DepthStencilState {
            format: PROGRAM_DEPTH_FORMAT,
            depth_write_enabled: Some(depth.write),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: multisample(spec.sample_count),
        multiview_mask: None,
        cache: None,
    })
}

/// The multisample state of a pipeline whose attachments hold
/// `sample_count` samples per texel: every sample written, and no
/// alpha-to-coverage.
fn multisample(sample_count: u32) -> wgpu::MultisampleState {
    wgpu::MultisampleState { count: sample_count, ..wgpu::MultisampleState::default() }
}

/// One authored compute pipeline: the authored entry point over the
/// same uniform and sampled-input groups render stages use, plus the
/// resident geometry storage buffers at group 2.
pub struct ProgramComputePipelineSpec<'a> {
    pub module: &'a wgpu::ShaderModule,
    pub entry_point: &'a str,
    pub uniform_layout: &'a wgpu::BindGroupLayout,
    pub inputs_layout: &'a wgpu::BindGroupLayout,
    pub storage_layout: &'a wgpu::BindGroupLayout,
}

#[must_use]
pub fn build_program_compute_pipeline(
    device: &wgpu::Device,
    spec: &ProgramComputePipelineSpec<'_>,
) -> wgpu::ComputePipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether program compute pipeline layout"),
        bind_group_layouts: &[Some(spec.uniform_layout), Some(spec.inputs_layout), Some(spec.storage_layout)],
        immediate_size: 0,
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("aether program compute pipeline"),
        layout: Some(&pipeline_layout),
        module: spec.module,
        entry_point: Some(spec.entry_point),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

/// Create one depth transient for the program transient pool
/// (ADR-0171, ADR-0246 decision 9): a `Depth32Float` attachment of
/// `sample_count` samples per texel that draw passes clear and test
/// against, beside a color attachment of the same size or, under a
/// depth-only pass, alone. A single-sample one is also a texture a
/// later pass binds — `RENDER_ATTACHMENT | TEXTURE_BINDING`, the rule
/// [`create_program_transient`] has — so a slot that is read and one
/// that is only attached are one pool class. A multisampled one is an
/// attachment only: it can be neither compared nor sampled, and a
/// depth attachment is never resolved.
#[must_use]
pub fn create_program_depth_transient(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    sample_count: u32,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("aether program depth transient"),
        size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count,
        dimension: wgpu::TextureDimension::D2,
        format: PROGRAM_DEPTH_FORMAT,
        usage: transient_usage(sample_count),
        view_formats: &[],
    })
}

/// What a pooled color or depth transient of `sample_count` samples per
/// texel is created for: a single-sample one is attached and bound, and
/// a multisampled one is attached only.
fn transient_usage(sample_count: u32) -> wgpu::TextureUsages {
    if sample_count > 1 {
        wgpu::TextureUsages::RENDER_ATTACHMENT
    } else {
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
    }
}

/// What one pooled transient texture is created as: its size, its
/// format, and how many samples each texel holds. Two textures of one
/// spec are interchangeable, which is what lets a pool key on it.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ProgramTransientSpec {
    pub width: u32,
    pub height: u32,
    pub format: wgpu::TextureFormat,
    pub sample_count: u32,
}

/// Create one transient intermediate texture for the program transient
/// pool (ADR-0170), with no CPU staging. A single-sample transient is a
/// render target program passes write and later passes sample —
/// `RENDER_ATTACHMENT | TEXTURE_BINDING` — and is also what a
/// multisampled one resolves into. A multisampled transient is a render
/// attachment only: passes attach it, and a pass reads the
/// single-sample texture it was resolved into. Content is defined by
/// the executor's clear-on-first-write policy, so no clear pass is
/// recorded here.
#[must_use]
pub fn create_program_transient(device: &wgpu::Device, spec: ProgramTransientSpec) -> wgpu::Texture {
    let usage = transient_usage(spec.sample_count);
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("aether program transient"),
        size: wgpu::Extent3d { width: spec.width.max(1), height: spec.height.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: spec.sample_count,
        dimension: wgpu::TextureDimension::D2,
        format: spec.format,
        usage,
        view_formats: &[],
    })
}

/// Where one recorded pass writes its GPU timestamps
/// (iamacoffeepot/aether#4423). Both indices name queries in the same
/// set, so the pass's span is one subtraction after the caller resolves
/// it; the caller owns the set, the resolve, and the readback.
///
/// A repeated pass brackets the whole repeat rather than each iteration:
/// the first iteration carries `beginning` alone, the last carries `end`
/// alone, and the iterations between carry no timestamps at all — which
/// is why each index is optional. wgpu rejects a `Some` timestamp write
/// with both indices `None`, so a pass with neither passes no
/// `timestamps` at all.
#[derive(Copy, Clone)]
pub struct PassTimestamps<'a> {
    pub query_set: &'a wgpu::QuerySet,
    pub beginning: Option<u32>,
    pub end: Option<u32>,
}

impl<'a> PassTimestamps<'a> {
    /// Lower into the render-pass descriptor's field. A method rather
    /// than a free mapper so both recorders spell it once.
    #[must_use]
    pub fn writes(self) -> wgpu::RenderPassTimestampWrites<'a> {
        wgpu::RenderPassTimestampWrites {
            query_set: self.query_set,
            beginning_of_pass_write_index: self.beginning,
            end_of_pass_write_index: self.end,
        }
    }

    /// Lower into a compute-pass descriptor's timestamp field.
    #[must_use]
    pub fn compute_writes(self) -> wgpu::ComputePassTimestampWrites<'a> {
        wgpu::ComputePassTimestampWrites {
            query_set: self.query_set,
            beginning_of_pass_write_index: self.beginning,
            end_of_pass_write_index: self.end,
        }
    }
}

/// One recorded program pass iteration: the pass's pipeline, the slot
/// view it renders into, the single-sample view a multisampled target
/// resolves into when this iteration ends (`None` for a single-sample
/// target, and for a multisampled one nothing reads before it is
/// written again), whether this is the dispatch's first write to that
/// slot (clear to transparent black) or a later one (load), and the two
/// bind groups — the uniform window at group 0 (bound at
/// `uniform_offset` into the dispatch's staged uniform buffer) and the
/// input pairs at group 1.
pub struct ProgramPassDraw<'a> {
    pub pipeline: &'a wgpu::RenderPipeline,
    pub target_view: &'a wgpu::TextureView,
    pub resolve_target: Option<&'a wgpu::TextureView>,
    pub clear: bool,
    pub uniform_bind_group: &'a wgpu::BindGroup,
    pub uniform_offset: u32,
    pub inputs_bind_group: &'a wgpu::BindGroup,
    /// GPU timestamps to bracket this iteration with, or `None` when the
    /// per-pass timing instrument is not running.
    pub timestamps: Option<PassTimestamps<'a>>,
}

/// Record one program pass iteration into `encoder`: a fullscreen
/// triangle through the pass pipeline into the target attachment.
pub fn record_program_pass(encoder: &mut wgpu::CommandEncoder, draw: &ProgramPassDraw<'_>) {
    let load = if draw.clear {
        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
    } else {
        wgpu::LoadOp::Load
    };
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("aether program pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: draw.target_view,
            resolve_target: draw.resolve_target,
            depth_slice: None,
            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: draw.timestamps.map(PassTimestamps::writes),
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(draw.pipeline);
    pass.set_bind_group(0, draw.uniform_bind_group, &[draw.uniform_offset]);
    pass.set_bind_group(1, draw.inputs_bind_group, &[]);
    pass.draw(0..3, 0..1);
}

/// The depth attachment of one recorded draw pass iteration: the pooled
/// transient's view, and whether this iteration is the dispatch's first
/// reference to that slot (clear to the far plane) or a later one
/// (load, so consecutive passes sharing a slot agree on occlusion).
pub struct ProgramDepthAttachment<'a> {
    pub view: &'a wgpu::TextureView,
    pub clear: bool,
}

/// The color attachment of one recorded draw pass iteration: the color
/// slot `view` it renders into, the single-sample view a multisampled
/// color slot resolves into when this iteration ends (`None` when it
/// does not resolve), and whether the pass's declared load semantic
/// clears the slot to transparent black or loads it. The three travel
/// together because a pass with no color attachment has none of them.
pub struct ProgramColorAttachment<'a> {
    pub view: &'a wgpu::TextureView,
    pub resolve_target: Option<&'a wgpu::TextureView>,
    pub clear: bool,
}

/// What every recorded draw pass iteration opens with (ADR-0171,
/// ADR-0246): the pass's pipeline, its color attachment (`None` for a
/// depth-only pass, which opens a render pass with no color
/// attachments), an optional depth attachment of the pass's sample
/// count, and the group-0 uniform window and group-1 input pairs. A
/// pass opens with at least one of the two attachments.
pub struct ProgramDrawPassOpen<'a> {
    pub pipeline: &'a wgpu::RenderPipeline,
    pub color: Option<ProgramColorAttachment<'a>>,
    pub depth: Option<ProgramDepthAttachment<'a>>,
    pub uniform_bind_group: &'a wgpu::BindGroup,
    pub uniform_offset: u32,
    pub inputs_bind_group: &'a wgpu::BindGroup,
    /// GPU timestamps to bracket this iteration with, or `None` when the
    /// per-pass timing instrument is not running.
    pub timestamps: Option<PassTimestamps<'a>>,
}

/// Begin one draw pass iteration in `encoder` and hand the open pass
/// back with its pipeline and both bind groups set, for the caller to
/// bind vertex and index buffers and issue draws into. The attachments'
/// clears happen when the pass ends, whether or not anything is drawn.
pub fn begin_program_draw_pass<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    open: &ProgramDrawPassOpen<'_>,
) -> wgpu::RenderPass<'e> {
    // An `Option` as a slice is the attachment list itself: one color
    // attachment, or none for a depth-only pass.
    let color_attachment = open.color.as_ref().map(|color| {
        let load = if color.clear {
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
        } else {
            wgpu::LoadOp::Load
        };
        Some(wgpu::RenderPassColorAttachment {
            view: color.view,
            resolve_target: color.resolve_target,
            depth_slice: None,
            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
        })
    });
    let depth_attachment = open.depth.as_ref().map(|depth| {
        let load = if depth.clear {
            wgpu::LoadOp::Clear(1.0)
        } else {
            wgpu::LoadOp::Load
        };
        wgpu::RenderPassDepthStencilAttachment {
            view: depth.view,
            depth_ops: Some(wgpu::Operations { load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }
    });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("aether program draw pass"),
        color_attachments: color_attachment.as_slice(),
        depth_stencil_attachment: depth_attachment,
        timestamp_writes: open.timestamps.map(PassTimestamps::writes),
        occlusion_query_set: None,
        multiview_mask: None,
    });

    pass.set_pipeline(open.pipeline);
    pass.set_bind_group(0, open.uniform_bind_group, &[open.uniform_offset]);
    pass.set_bind_group(1, open.inputs_bind_group, &[]);
    pass
}

/// One recorded draw pass iteration over a bound geometry (ADR-0171):
/// what the pass opens with, the geometry's realized buffers, and how
/// its draw obtains its arguments.
pub struct ProgramDrawPass<'a> {
    pub open: ProgramDrawPassOpen<'a>,
    pub vertex_buffer: &'a wgpu::Buffer,
    pub index_buffer: &'a wgpu::Buffer,
    pub command: ProgramDrawCommand<'a>,
}

/// How a recorded authored draw obtains its indexed draw arguments.
pub enum ProgramDrawCommand<'a> {
    Direct { index_count: u32 },
    Indirect { buffer: &'a wgpu::Buffer },
}

/// Record one draw pass iteration into `encoder`: an indexed
/// triangle-list draw of the bound geometry through the pass pipeline
/// into the pass's attachments. A geometry with
/// no indices still runs the pass — its clears are the caller's
/// declaration — and issues no draw.
pub fn record_program_draw_pass(encoder: &mut wgpu::CommandEncoder, draw: &ProgramDrawPass<'_>) {
    let mut pass = begin_program_draw_pass(encoder, &draw.open);
    if matches!(draw.command, ProgramDrawCommand::Direct { index_count: 0 }) {
        return;
    }

    pass.set_vertex_buffer(0, draw.vertex_buffer.slice(..));
    pass.set_index_buffer(draw.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
    match draw.command {
        ProgramDrawCommand::Direct { index_count } => pass.draw_indexed(0..index_count, 0, 0..1),
        ProgramDrawCommand::Indirect { buffer } => pass.draw_indexed_indirect(buffer, 0),
    }
}

/// One authored compute-pass iteration over the shared group-0 uniform
/// window, group-1 sampled inputs, and group-2 resident buffers.
pub struct ProgramComputePass<'a> {
    pub pipeline: &'a wgpu::ComputePipeline,
    pub uniform_bind_group: &'a wgpu::BindGroup,
    pub uniform_offset: u32,
    pub inputs_bind_group: &'a wgpu::BindGroup,
    pub storage_bind_group: &'a wgpu::BindGroup,
    pub workgroups: [u32; 3],
    pub timestamps: Option<PassTimestamps<'a>>,
}

/// Record one authored compute-pass iteration. Ending this pass before a
/// later render pass gives wgpu the storage-to-vertex/index/indirect
/// ordering transition inside the same command encoder.
pub fn record_program_compute_pass(encoder: &mut wgpu::CommandEncoder, dispatch: &ProgramComputePass<'_>) {
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some("aether program compute pass"),
        timestamp_writes: dispatch.timestamps.map(PassTimestamps::compute_writes),
    });
    pass.set_pipeline(dispatch.pipeline);
    pass.set_bind_group(0, dispatch.uniform_bind_group, &[dispatch.uniform_offset]);
    pass.set_bind_group(1, dispatch.inputs_bind_group, &[]);
    pass.set_bind_group(2, dispatch.storage_bind_group, &[]);
    pass.dispatch_workgroups(dispatch.workgroups[0], dispatch.workgroups[1], dispatch.workgroups[2]);
}
