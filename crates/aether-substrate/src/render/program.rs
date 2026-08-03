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

/// Group-0 layout for a pass's uniform window: one fragment-visible
/// uniform buffer with a dynamic offset, so a repeated pass rebinds its
/// per-iteration window as an offset rather than a fresh bind group.
/// `bound_bytes` is the window length the pass binds (at least the
/// shader's declared block size — the render cap validates that at
/// register time).
#[must_use]
pub fn program_uniform_layout(device: &wgpu::Device, bound_bytes: u64) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("aether program uniform bind group layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
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
#[must_use]
pub fn program_inputs_layout(device: &wgpu::Device, filterable: &[bool]) -> wgpu::BindGroupLayout {
    let mut entries = Vec::with_capacity(filterable.len() * 2);
    let mut base = 0u32;
    for &filterable in filterable {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: base,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: base + 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
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
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(draw.pipeline);
    pass.set_bind_group(0, draw.uniform_bind_group, &[draw.uniform_offset]);
    pass.set_bind_group(1, draw.inputs_bind_group, &[]);
    pass.draw(0..3, 0..1);
}
