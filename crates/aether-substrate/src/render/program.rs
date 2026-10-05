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

/// How the texture at one program input is viewed, which is also the
/// type the shader declares for it.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProgramInputView {
    /// `texture_2d<f32>`.
    Plain,
    /// `texture_2d_array<f32>`.
    Array,
}

/// The sampler that accompanies one program input's texture.
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

/// One input of a program pass, as its group-1 layout needs it.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ProgramInput {
    pub view: ProgramInputView,
    pub sampler: ProgramInputSampler,
}

/// Group-1 layout for a pass's inputs, in slot order. Input `n` is the
/// texture at `@binding(2n)` and, when it has one, the sampler at
/// `@binding(2n + 1)`. An input without a sampler leaves `2n + 1` out
/// of the layout, so the inputs after it keep their binding numbers
/// whatever the inputs before them declare.
///
/// The texture entry is filterable exactly when its sampler is
/// [`ProgramInputSampler::Filtering`]. Declaring the other two
/// unfilterable is what lets a texture of any format bind there: a
/// filterable texture satisfies an unfilterable entry, and the reverse
/// does not hold.
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
        let view_dimension = match description.view {
            ProgramInputView::Plain => wgpu::TextureViewDimension::D2,
            ProgramInputView::Array => wgpu::TextureViewDimension::D2Array,
        };
        let filterable = description.sampler == ProgramInputSampler::Filtering;
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: base,
            visibility,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension,
                multisampled: false,
            },
            count: None,
        });

        let sampler = match description.sampler {
            ProgramInputSampler::Filtering => wgpu::SamplerBindingType::Filtering,
            ProgramInputSampler::NonFiltering => wgpu::SamplerBindingType::NonFiltering,
            ProgramInputSampler::None => continue,
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
/// rendering into a color attachment of `color_format`. `blend` is
/// `Some` for blendable color formats (alpha over the target) and `None`
/// for `R32Float`, which core WebGPU cannot blend — the pass replaces
/// instead.
pub struct ProgramPipelineSpec<'a> {
    pub vertex_module: &'a wgpu::ShaderModule,
    pub fragment_module: &'a wgpu::ShaderModule,
    pub entry_point: &'a str,
    pub color_format: wgpu::TextureFormat,
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
        multisample: wgpu::MultisampleState::default(),
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

/// One draw pass's pipeline shape (ADR-0171, ADR-0246): the authored
/// module's vertex and fragment entry points over `vertex_buffers`, in
/// buffer-slot order, into a color attachment of `color_format`. A pass
/// over one bound geometry has one per-vertex buffer; a draw-sets pass
/// adds a per-instance buffer at slot 1. `cull_mode` is `None` for a
/// pass that draws both windings. `depth` is the state a declared depth
/// transient attaches under; a pass declaring none rasterizes in draw
/// order.
pub struct ProgramDrawPipelineSpec<'a> {
    pub module: &'a wgpu::ShaderModule,
    pub vertex_entry_point: &'a str,
    pub fragment_entry_point: &'a str,
    pub vertex_buffers: &'a [ProgramVertexBuffer<'a>],
    pub cull_mode: Option<wgpu::Face>,
    pub color_format: wgpu::TextureFormat,
    pub blend: Option<wgpu::BlendState>,
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
    let vertex_buffers: Vec<wgpu::VertexBufferLayout<'_>> = spec
        .vertex_buffers
        .iter()
        .map(|buffer| wgpu::VertexBufferLayout {
            array_stride: buffer.stride_bytes,
            step_mode: buffer.step_mode,
            attributes: buffer.attributes,
        })
        .collect();
    let fragment_targets = [Some(wgpu::ColorTargetState {
        format: spec.color_format,
        blend: spec.blend,
        write_mask: wgpu::ColorWrites::ALL,
    })];
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
            targets: &fragment_targets,
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
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
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
/// (ADR-0171): a `Depth32Float` attachment draw passes clear and test
/// against. Render-attachment only — nothing samples it, and the pass
/// that shares it does so by attaching it again.
#[must_use]
pub fn create_program_depth_transient(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("aether program depth transient"),
        size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: PROGRAM_DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    })
}

/// Create one transient intermediate texture for the program transient
/// pool (ADR-0170): a render target program passes write and later
/// passes sample — `RENDER_ATTACHMENT | TEXTURE_BINDING`, no CPU
/// staging. Content is defined by the executor's clear-on-first-write
/// policy, so no clear pass is recorded here.
#[must_use]
pub fn create_program_transient(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("aether program transient"),
        size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
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
/// view it renders into, whether this is the dispatch's first write to
/// that slot (clear to transparent black) or a later one (load), and
/// the two bind groups — the uniform window at group 0 (bound at
/// `uniform_offset` into the dispatch's staged uniform buffer) and the
/// input pairs at group 1.
pub struct ProgramPassDraw<'a> {
    pub pipeline: &'a wgpu::RenderPipeline,
    pub target_view: &'a wgpu::TextureView,
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
            resolve_target: None,
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

/// What every recorded draw pass iteration opens with (ADR-0171,
/// ADR-0246): the pass's pipeline, the color slot view it renders into
/// under the pass's declared load semantic, an optional depth
/// attachment, and the group-0 uniform window and group-1 input pairs.
pub struct ProgramDrawPassOpen<'a> {
    pub pipeline: &'a wgpu::RenderPipeline,
    pub target_view: &'a wgpu::TextureView,
    pub clear_color: bool,
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
    let load = if open.clear_color {
        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
    } else {
        wgpu::LoadOp::Load
    };
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
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: open.target_view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
        })],
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
/// into the color attachment, optionally depth-tested. A geometry with
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
