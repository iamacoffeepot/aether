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

/// Group-1 layout for a pass's input slots: one texture / sampler pair
/// per input, in slot order — input `n` is texture `@binding(2n)` plus
/// sampler `@binding(2n + 1)`. `filterable` carries each input's format
/// filterability: a filterable input gets the `Float { filterable: true }`
/// / `Filtering` pair, a data-plane input (`R32Float`) the non-filtering
/// pair, matching the shared [`super::TextureBindings`] convention.
///
/// Visible to both stages, for the same reason the uniform window is
/// (ADR-0172): a draw pass whose vertex stage displaces geometry by a
/// data plane — the ink ribbons reading their own visibility field —
/// must `textureLoad` that plane before the rasterizer exists to have a
/// fragment stage. Sampling with implicit derivatives stays a
/// fragment-only operation by WGSL's own rule, so widening the layout
/// grants a vertex stage `textureLoad` / `textureSampleLevel` and
/// nothing more, and visibility a stage does not use costs nothing.
#[must_use]
pub fn program_inputs_layout(
    device: &wgpu::Device,
    filterable: &[bool],
    visibility: wgpu::ShaderStages,
) -> wgpu::BindGroupLayout {
    let mut entries = Vec::with_capacity(filterable.len() * 2);
    let mut base = 0u32;
    for &filterable in filterable {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: base,
            visibility,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: base + 1,
            visibility,
            ty: wgpu::BindingType::Sampler(if filterable {
                wgpu::SamplerBindingType::Filtering
            } else {
                wgpu::SamplerBindingType::NonFiltering
            }),
            count: None,
        });
        base += 2;
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

/// Build one program pass pipeline: the shared fullscreen vertex stage
/// over the authored module's fragment `entry_point`, rendering into a
/// color attachment of `color_format`. `blend` is `Some` for blendable
/// color formats (alpha over the target) and `None` for `R32Float`,
/// which core WebGPU cannot blend — the pass replaces instead. No
/// vertex buffers, no depth: program passes are pure image work.
// Eight arguments mirror the same all-in-one shape `realize_texture` and
// `record_quad_overlay_pass` use; bundling into a struct for the one
// register call site adds no clarity.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_program_pipeline(
    device: &wgpu::Device,
    vertex_module: &wgpu::ShaderModule,
    fragment_module: &wgpu::ShaderModule,
    entry_point: &str,
    color_format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
    uniform_layout: &wgpu::BindGroupLayout,
    inputs_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
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

/// One draw pass's pipeline shape (ADR-0171): the authored module's
/// vertex and fragment entry points over a bound geometry, into a color
/// attachment of `color_format`. `vertex_attributes` and
/// `vertex_stride_bytes` come from the geometry slot's declared layout,
/// which the render cap has already checked the vertex stage's
/// interface against. `depth` builds the `LessEqual` depth-write state
/// a declared depth transient attaches to; a pass declaring none
/// rasterizes in draw order.
pub struct ProgramDrawPipelineSpec<'a> {
    pub module: &'a wgpu::ShaderModule,
    pub vertex_entry_point: &'a str,
    pub fragment_entry_point: &'a str,
    pub vertex_stride_bytes: u64,
    pub vertex_attributes: &'a [wgpu::VertexAttribute],
    pub color_format: wgpu::TextureFormat,
    pub blend: Option<wgpu::BlendState>,
    pub depth: bool,
    pub uniform_layout: &'a wgpu::BindGroupLayout,
    pub inputs_layout: &'a wgpu::BindGroupLayout,
}

/// Build one draw pass pipeline from its [`ProgramDrawPipelineSpec`].
/// Culling stays off — winding is the authoring actor's business, and
/// the substrate has no view on which side of a face it is painting.
#[must_use]
pub fn build_program_draw_pipeline(device: &wgpu::Device, spec: &ProgramDrawPipelineSpec<'_>) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether program draw pipeline layout"),
        bind_group_layouts: &[Some(spec.uniform_layout), Some(spec.inputs_layout)],
        immediate_size: 0,
    });
    let vertex_buffers = [wgpu::VertexBufferLayout {
        array_stride: spec.vertex_stride_bytes,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: spec.vertex_attributes,
    }];
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
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: spec.depth.then(|| wgpu::DepthStencilState {
            format: PROGRAM_DEPTH_FORMAT,
            depth_write_enabled: Some(true),
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

/// One recorded draw pass iteration (ADR-0171): the pass's pipeline, the
/// color slot view it renders into under the pass's declared load
/// semantic, an optional depth attachment, the group-0 uniform window
/// and group-1 input pairs, and the bound geometry's realized buffers.
pub struct ProgramDrawPass<'a> {
    pub pipeline: &'a wgpu::RenderPipeline,
    pub target_view: &'a wgpu::TextureView,
    pub clear_color: bool,
    pub depth: Option<ProgramDepthAttachment<'a>>,
    pub uniform_bind_group: &'a wgpu::BindGroup,
    pub uniform_offset: u32,
    pub inputs_bind_group: &'a wgpu::BindGroup,
    pub vertex_buffer: &'a wgpu::Buffer,
    pub index_buffer: &'a wgpu::Buffer,
    pub command: ProgramDrawCommand<'a>,
    /// GPU timestamps to bracket this iteration with, or `None` when the
    /// per-pass timing instrument is not running.
    pub timestamps: Option<PassTimestamps<'a>>,
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
    let load = if draw.clear_color {
        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
    } else {
        wgpu::LoadOp::Load
    };
    let depth_attachment = draw.depth.as_ref().map(|depth| wgpu::RenderPassDepthStencilAttachment {
        view: depth.view,
        depth_ops: Some(wgpu::Operations {
            load: if depth.clear {
                wgpu::LoadOp::Clear(1.0)
            } else {
                wgpu::LoadOp::Load
            },
            store: wgpu::StoreOp::Store,
        }),
        stencil_ops: None,
    });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("aether program draw pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: draw.target_view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: depth_attachment,
        timestamp_writes: draw.timestamps.map(PassTimestamps::writes),
        occlusion_query_set: None,
        multiview_mask: None,
    });
    if matches!(draw.command, ProgramDrawCommand::Direct { index_count: 0 }) {
        return;
    }
    pass.set_pipeline(draw.pipeline);
    pass.set_bind_group(0, draw.uniform_bind_group, &[draw.uniform_offset]);
    pass.set_bind_group(1, draw.inputs_bind_group, &[]);
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
