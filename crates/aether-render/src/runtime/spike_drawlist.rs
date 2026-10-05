//! SPIKE-ONLY (branch `spike/mesh-draw-path`, never on `main`): a draw-list
//! pass executor beside the authored-program executor. One
//! `begin_render_pass` per `SpikeDrawPass`, then one `draw_indexed` per
//! list entry, setting buffers and bind groups only when they change. It
//! shares the render cap's device, texture registry, geometry registry and
//! frame encoder, and owns instance buffers, texture arrays, draw pipelines
//! and retained draw lists. Spike quality throughout: no error scopes per
//! pass, no device-loss recovery, no destroy for most resources.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use aether_substrate::render::{DEPTH_FORMAT, MSAA_SAMPLE_COUNT, Targets};

use super::geometry::{GeometryRegistry, wgpu_vertex_attributes};
use super::pipeline::RenderGpu;
use super::texture::{TextureRegistry, wgpu_texture_format};
use crate::kinds::{TextureUsage, VertexAttribute, vertex_stride_bytes};
use crate::spike_kinds::{
    SPIKE_FRAME_TARGET, SpikeCreateDrawList, SpikeCreateInstances, SpikeCreatePipeline, SpikeCreateTextureArray,
    SpikeCreated, SpikeCull, SpikeDraw, SpikeDrawPass, SpikePatchDrawList, SpikeTextures, SpikeUpdateInstances,
};
use crate::spike_probe;

const INDIRECT_ARGS_BYTES: usize = 20;

struct Instances {
    layout: Vec<VertexAttribute>,
    stride: usize,
    count: u32,
    buffer: wgpu::Buffer,
}

struct DrawPipeline {
    pipeline: wgpu::RenderPipeline,
    vertex_layout: Vec<VertexAttribute>,
    instance_layout: Vec<VertexAttribute>,
    textures: SpikeTextures,
    format: wgpu::TextureFormat,
    to_frame: bool,
    samples: u32,
    depth: bool,
    uniform_bytes: usize,
    uniform_buffer: wgpu::Buffer,
    uniform_group: wgpu::BindGroup,
}

#[derive(Clone, Copy)]
struct ResolvedDraw {
    geometry: u32,
    instances: u32,
    bind: u32,
    first_index: u32,
    index_count: u32,
    base_vertex: i32,
    first_instance: u32,
    instance_count: u32,
}

/// A draw list with every id replaced by a slot in a table of the wgpu
/// handles it names. Holding the handles is what keeps a retained list's
/// resources alive after their registry entries are destroyed.
#[derive(Default)]
struct ResolvedList {
    geometries: Vec<(wgpu::Buffer, wgpu::Buffer)>,
    geometry_meta: Vec<(u32, u32)>,
    geometry_slots: HashMap<u32, u32>,
    instances: Vec<wgpu::Buffer>,
    instance_counts: Vec<u32>,
    instance_slots: HashMap<u32, u32>,
    binds: Vec<wgpu::BindGroup>,
    bind_slots: HashMap<u32, u32>,
    draws: Vec<ResolvedDraw>,
    indirect: Option<wgpu::Buffer>,
    pipeline_id: u32,
}

struct Shared {
    per_draw_layout: wgpu::BindGroupLayout,
    array_layout: wgpu::BindGroupLayout,
    repeat_sampler: wgpu::Sampler,
}

#[derive(Default)]
pub struct SpikeDrawLists {
    next_id: u32,
    shared: Option<Shared>,
    instances: HashMap<u32, Instances>,
    arrays: HashMap<u32, wgpu::BindGroup>,
    pipelines: HashMap<u32, DrawPipeline>,
    lists: HashMap<u32, ResolvedList>,
    /// One group-1 bind group per registry texture id, shared by every
    /// per-draw pipeline (they share one layout).
    texture_binds: HashMap<u32, wgpu::BindGroup>,
    /// Executor-owned attachments keyed by `(width, height, format, samples)`.
    attachments: HashMap<(u32, u32, wgpu::TextureFormat, u32), wgpu::TextureView>,
    scratch: ResolvedList,
}

fn add(counter: &std::sync::atomic::AtomicU64, started: Instant) {
    counter.fetch_add(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
}

fn texture_layout(device: &wgpu::Device, dimension: wgpu::TextureViewDimension) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("spike draw-list textures"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: dimension,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn shared<'a>(slot: &'a mut Option<Shared>, device: &wgpu::Device) -> &'a Shared {
    slot.get_or_insert_with(|| Shared {
        per_draw_layout: texture_layout(device, wgpu::TextureViewDimension::D2),
        array_layout: texture_layout(device, wgpu::TextureViewDimension::D2Array),
        repeat_sampler: device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("spike repeat sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        }),
    })
}

/// What `resolve` checks a list against.
struct ResolveCtx<'a> {
    gpu: &'a RenderGpu,
    textures: &'a mut TextureRegistry,
    geometries: &'a mut GeometryRegistry,
    pipeline: &'a DrawPipeline,
    instances: &'a HashMap<u32, Instances>,
    texture_binds: &'a mut HashMap<u32, wgpu::BindGroup>,
    shared: &'a Shared,
}

/// Resolve `draws` onto the end of `list.draws`, checking every way an
/// entry can be invalid that is checkable without reading index data.
/// A resource is looked up, checked and realized once per distinct id
/// per list (the slot maps); a run of draws naming the same id as the
/// one before skips even the map lookup.
fn resolve(list: &mut ResolvedList, draws: &[SpikeDraw], ctx: &mut ResolveCtx<'_>) -> Result<(), String> {
    let per_draw = ctx.pipeline.textures == SpikeTextures::PerDraw;
    let mut last = (u32::MAX, u32::MAX, u32::MAX);
    let mut slots = (0u32, 0u32, 0u32);
    list.draws.reserve(draws.len());
    for (index, draw) in draws.iter().enumerate() {
        if draw.geometry_id != last.0 {
            slots.0 = match list.geometry_slots.get(&draw.geometry_id) {
                Some(slot) => *slot,
                None => {
                    let entry = ctx
                        .geometries
                        .entries
                        .get_mut(&draw.geometry_id)
                        .ok_or_else(|| format!("draw {index}: unknown geometry id {}", draw.geometry_id))?;
                    if entry.layout != ctx.pipeline.vertex_layout {
                        return Err(format!("draw {index}: geometry {} layout mismatch", draw.geometry_id));
                    }
                    entry.ensure_realized(&ctx.gpu.device, &ctx.gpu.queue);
                    let vertex_count = entry.vertex_bytes().len() / vertex_stride_bytes(&entry.layout);
                    let realized = entry.realized.as_ref().expect("realized above");
                    let slot = list.geometries.len() as u32;
                    list.geometries.push((realized.vertex_buffer.clone(), realized.index_buffer.clone()));
                    list.geometry_meta.push((realized.index_count, vertex_count as u32));
                    list.geometry_slots.insert(draw.geometry_id, slot);
                    slot
                }
            };
            last.0 = draw.geometry_id;
        }
        let (index_count, vertex_count) = list.geometry_meta[slots.0 as usize];
        let index_end = draw.first_index.checked_add(draw.index_count);
        if !index_end.is_some_and(|end| end <= index_count) {
            return Err(format!("draw {index}: index range past geometry {} ({index_count})", draw.geometry_id));
        }
        // Not the whole check: the largest index in the range plus
        // `base_vertex` must be below the vertex count, which needs the
        // range's maximum index. wgpu clamps the read on the GPU instead.
        if draw.base_vertex >= vertex_count {
            return Err(format!("draw {index}: base vertex past geometry {} ({vertex_count})", draw.geometry_id));
        }

        if draw.instances_id != last.1 {
            slots.1 = match list.instance_slots.get(&draw.instances_id) {
                Some(slot) => *slot,
                None => {
                    let entry = ctx
                        .instances
                        .get(&draw.instances_id)
                        .ok_or_else(|| format!("draw {index}: unknown instances id {}", draw.instances_id))?;
                    if entry.layout != ctx.pipeline.instance_layout {
                        return Err(format!("draw {index}: instances {} layout mismatch", draw.instances_id));
                    }
                    let slot = list.instances.len() as u32;
                    list.instances.push(entry.buffer.clone());
                    list.instance_counts.push(entry.count);
                    list.instance_slots.insert(draw.instances_id, slot);
                    slot
                }
            };
            last.1 = draw.instances_id;
        }
        let instance_end = draw.first_instance.checked_add(draw.instance_count);
        let instance_count = list.instance_counts[slots.1 as usize];
        if !instance_end.is_some_and(|end| end <= instance_count) {
            return Err(format!("draw {index}: instance range past instances {} ({instance_count})", draw.instances_id));
        }

        if per_draw && draw.texture_id != last.2 {
            slots.2 = match list.bind_slots.get(&draw.texture_id) {
                Some(slot) => *slot,
                None => {
                    let group = match ctx.texture_binds.get(&draw.texture_id) {
                        Some(group) => group.clone(),
                        None => {
                            let entry = ctx
                                .textures
                                .entries
                                .get_mut(&draw.texture_id)
                                .ok_or_else(|| format!("draw {index}: unknown texture id {}", draw.texture_id))?;
                            if !entry.format.filterable() {
                                return Err(format!("draw {index}: texture {} is not filterable", draw.texture_id));
                            }
                            entry.ensure_realized(&ctx.gpu.device, &ctx.gpu.queue, &ctx.gpu.texture_bindings);
                            let view = entry
                                .realized
                                .as_ref()
                                .expect("realized above")
                                .texture()
                                .create_view(&wgpu::TextureViewDescriptor::default());
                            let group = ctx.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                                label: Some("spike per-draw texture"),
                                layout: &ctx.shared.per_draw_layout,
                                entries: &[
                                    wgpu::BindGroupEntry {
                                        binding: 0,
                                        resource: wgpu::BindingResource::TextureView(&view),
                                    },
                                    wgpu::BindGroupEntry {
                                        binding: 1,
                                        resource: wgpu::BindingResource::Sampler(&ctx.shared.repeat_sampler),
                                    },
                                ],
                            });
                            ctx.texture_binds.insert(draw.texture_id, group.clone());
                            spike_probe::BIND_GROUPS_CREATED.fetch_add(1, Ordering::Relaxed);
                            group
                        }
                    };
                    let slot = list.binds.len() as u32;
                    list.binds.push(group);
                    list.bind_slots.insert(draw.texture_id, slot);
                    slot
                }
            };
            last.2 = draw.texture_id;
        }

        list.draws.push(ResolvedDraw {
            geometry: slots.0,
            instances: slots.1,
            bind: slots.2,
            first_index: draw.first_index,
            index_count: draw.index_count,
            base_vertex: draw.base_vertex as i32,
            first_instance: draw.first_instance,
            instance_count: draw.instance_count,
        });
    }
    Ok(())
}

fn indirect_args(draws: &[ResolvedDraw]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(draws.len() * INDIRECT_ARGS_BYTES);
    for draw in draws {
        for word in [
            draw.index_count,
            draw.instance_count,
            draw.first_index,
            draw.base_vertex.cast_unsigned(),
            draw.first_instance,
        ] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    bytes
}

fn encode(
    pass: &mut wgpu::RenderPass<'_>,
    pipeline: &DrawPipeline,
    list: &ResolvedList,
    array: Option<&wgpu::BindGroup>,
) {
    pass.set_pipeline(&pipeline.pipeline);
    pass.set_bind_group(0, &pipeline.uniform_group, &[]);
    if let Some(array) = array {
        pass.set_bind_group(1, array, &[]);
    }
    let per_draw = pipeline.textures == SpikeTextures::PerDraw;
    if let (Some(indirect), Some(first)) = (&list.indirect, list.draws.first()) {
        let (vertices, indices) = &list.geometries[first.geometry as usize];
        pass.set_vertex_buffer(0, vertices.slice(..));
        pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.set_vertex_buffer(1, list.instances[first.instances as usize].slice(..));
        if per_draw {
            pass.set_bind_group(1, &list.binds[first.bind as usize], &[]);
        }
        pass.multi_draw_indexed_indirect(indirect, 0, list.draws.len() as u32);
        spike_probe::DRAWS.fetch_add(list.draws.len() as u64, Ordering::Relaxed);
        return;
    }
    let mut bound = (u32::MAX, u32::MAX, u32::MAX);
    let mut sets = 0u64;
    for draw in &list.draws {
        if draw.geometry != bound.0 {
            let (vertices, indices) = &list.geometries[draw.geometry as usize];
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            bound.0 = draw.geometry;
            sets += 2;
        }
        if draw.instances != bound.1 {
            pass.set_vertex_buffer(1, list.instances[draw.instances as usize].slice(..));
            bound.1 = draw.instances;
            sets += 1;
        }
        if per_draw && draw.bind != bound.2 {
            pass.set_bind_group(1, &list.binds[draw.bind as usize], &[]);
            bound.2 = draw.bind;
            sets += 1;
        }
        pass.draw_indexed(
            draw.first_index..draw.first_index + draw.index_count,
            draw.base_vertex,
            draw.first_instance..draw.first_instance + draw.instance_count,
        );
    }
    spike_probe::DRAWS.fetch_add(list.draws.len() as u64, Ordering::Relaxed);
    spike_probe::STATE_SETS.fetch_add(sets, Ordering::Relaxed);
}

fn attachment<'a>(
    cache: &'a mut HashMap<(u32, u32, wgpu::TextureFormat, u32), wgpu::TextureView>,
    device: &wgpu::Device,
    key: (u32, u32, wgpu::TextureFormat, u32),
) -> &'a wgpu::TextureView {
    cache.entry(key).or_insert_with(|| {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("spike draw-list attachment"),
                size: wgpu::Extent3d { width: key.0, height: key.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: key.3,
                dimension: wgpu::TextureDimension::D2,
                format: key.2,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    })
}

impl SpikeDrawLists {
    fn allocate(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    pub fn limits(gpu: &RenderGpu) -> String {
        let limits = gpu.device.limits();
        format!(
            "max_texture_array_layers={} max_texture_dimension_2d={} max_bind_groups={} \
             max_bindings_per_bind_group={} max_sampled_textures_per_shader_stage={} max_samplers_per_shader_stage={} \
             max_vertex_buffers={} max_vertex_attributes={} max_buffer_size={} max_uniform_buffer_binding_size={} \
             features={:?}",
            limits.max_texture_array_layers,
            limits.max_texture_dimension_2d,
            limits.max_bind_groups,
            limits.max_bindings_per_bind_group,
            limits.max_sampled_textures_per_shader_stage,
            limits.max_samplers_per_shader_stage,
            limits.max_vertex_buffers,
            limits.max_vertex_attributes,
            limits.max_buffer_size,
            limits.max_uniform_buffer_binding_size,
            gpu.device.features(),
        )
    }

    pub fn create_instances(&mut self, gpu: &RenderGpu, mail: SpikeCreateInstances) -> SpikeCreated {
        let Some(bytes) = mail.data.contiguous() else {
            return SpikeCreated::Err { error: "instance bytes are not resident".to_owned() };
        };
        let stride = vertex_stride_bytes(&mail.layout);
        if stride == 0 || bytes.len() % stride != 0 || bytes.is_empty() {
            return SpikeCreated::Err { error: "instance bytes do not divide by the layout stride".to_owned() };
        }
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spike instances"),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if let Some(error) = pollster::block_on(scope.pop()) {
            return SpikeCreated::Err { error: format!("instance buffer creation failed: {error}") };
        }
        gpu.queue.write_buffer(&buffer, 0, bytes);
        let id = self.allocate();
        self.instances
            .insert(id, Instances { layout: mail.layout, stride, count: (bytes.len() / stride) as u32, buffer });
        SpikeCreated::Ok { id }
    }

    pub fn update_instances(&mut self, gpu: &RenderGpu, mail: &SpikeUpdateInstances) {
        let started = Instant::now();
        let Some(entry) = self.instances.get(&mail.instances_id) else {
            tracing::warn!(target: "aether_render", "spike update_instances: unknown id");
            return;
        };
        let end = mail.first_instance as usize * entry.stride + mail.data.len();
        if mail.data.len() % entry.stride != 0 || end > entry.count as usize * entry.stride {
            tracing::warn!(target: "aether_render", "spike update_instances: range out of bounds");
            return;
        }
        gpu.queue.write_buffer(&entry.buffer, (mail.first_instance as usize * entry.stride) as u64, &mail.data);
        add(&spike_probe::UPDATE_NANOS, started);
    }

    pub fn create_texture_array(&mut self, gpu: &RenderGpu, mail: SpikeCreateTextureArray) -> SpikeCreated {
        let Some(bytes) = mail.pixels.contiguous() else {
            return SpikeCreated::Err { error: "array pixels are not resident".to_owned() };
        };
        if bytes.len() != mail.width as usize * mail.height as usize * mail.layers as usize * 4 {
            return SpikeCreated::Err { error: "array pixels are not width * height * layers * 4 bytes".to_owned() };
        }
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let size = wgpu::Extent3d { width: mail.width, height: mail.height, depth_or_array_layers: mail.layers };
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spike texture array"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        if let Some(error) = pollster::block_on(scope.pop()) {
            return SpikeCreated::Err { error: format!("texture array creation failed: {error}") };
        }
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(mail.width * 4),
                rows_per_image: Some(mail.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..wgpu::TextureViewDescriptor::default()
        });
        let shared = shared(&mut self.shared, &gpu.device);
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spike texture array"),
            layout: &shared.array_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&shared.repeat_sampler) },
            ],
        });
        let id = self.allocate();
        self.arrays.insert(id, group);
        SpikeCreated::Ok { id }
    }

    pub fn create_pipeline(&mut self, gpu: &RenderGpu, mail: SpikeCreatePipeline) -> SpikeCreated {
        let device = &gpu.device;
        let shared = shared(&mut self.shared, device);
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("spike draw-list shader"),
            source: wgpu::ShaderSource::Wgsl(mail.wgsl.as_str().into()),
        });
        let uniform_bytes = u64::from(mail.uniform_bytes.max(4));
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spike draw-list uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(uniform_bytes),
                },
                count: None,
            }],
        });
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spike draw-list uniforms"),
            size: uniform_bytes,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spike draw-list uniforms"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform_buffer.as_entire_binding() }],
        });
        let texture_layout = match mail.textures {
            SpikeTextures::None => None,
            SpikeTextures::PerDraw => Some(&shared.per_draw_layout),
            SpikeTextures::Array => Some(&shared.array_layout),
        };
        let mut group_layouts = vec![Some(&uniform_layout)];
        group_layouts.extend(texture_layout.map(Some));
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("spike draw-list pipeline layout"),
            bind_group_layouts: &group_layouts,
            immediate_size: 0,
        });
        let vertex_attributes = wgpu_vertex_attributes(&mail.vertex_layout);
        let instance_attributes = wgpu_vertex_attributes(&mail.instance_layout);
        let buffers = [
            wgpu::VertexBufferLayout {
                array_stride: vertex_stride_bytes(&mail.vertex_layout) as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &vertex_attributes,
            },
            wgpu::VertexBufferLayout {
                array_stride: vertex_stride_bytes(&mail.instance_layout) as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &instance_attributes,
            },
        ];
        let (format, samples) = if mail.to_frame {
            (gpu.color_format, MSAA_SAMPLE_COUNT)
        } else {
            (wgpu_texture_format(mail.target_format), mail.samples.max(1))
        };
        let targets = [Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("spike draw-list pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some(&mail.vertex_entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &buffers,
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(&mail.fragment_entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &targets,
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: match mail.cull {
                    SpikeCull::None => None,
                    SpikeCull::Back => Some(wgpu::Face::Back),
                },
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: mail.depth.then(|| wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState { count: samples, ..wgpu::MultisampleState::default() },
            multiview_mask: None,
            cache: None,
        });
        if let Some(error) = pollster::block_on(scope.pop()) {
            return SpikeCreated::Err { error: format!("pipeline creation failed: {error}") };
        }
        let id = self.allocate();
        self.pipelines.insert(
            id,
            DrawPipeline {
                pipeline,
                vertex_layout: mail.vertex_layout,
                instance_layout: mail.instance_layout,
                textures: mail.textures,
                format,
                to_frame: mail.to_frame,
                samples,
                depth: mail.depth,
                uniform_bytes: mail.uniform_bytes as usize,
                uniform_buffer,
                uniform_group,
            },
        );
        SpikeCreated::Ok { id }
    }

    pub fn create_draw_list(
        &mut self,
        gpu: &RenderGpu,
        textures: &mut TextureRegistry,
        geometries: &mut GeometryRegistry,
        mail: &SpikeCreateDrawList,
    ) -> SpikeCreated {
        let started = Instant::now();
        let Some(pipeline) = self.pipelines.get(&mail.pipeline_id) else {
            return SpikeCreated::Err { error: format!("unknown pipeline id {}", mail.pipeline_id) };
        };
        let mut list = ResolvedList { pipeline_id: mail.pipeline_id, ..ResolvedList::default() };
        let mut ctx = ResolveCtx {
            gpu,
            textures,
            geometries,
            pipeline,
            instances: &self.instances,
            texture_binds: &mut self.texture_binds,
            shared: shared(&mut self.shared, &gpu.device),
        };
        if let Err(error) = resolve(&mut list, &mail.draws, &mut ctx) {
            return SpikeCreated::Err { error };
        }
        if mail.indirect {
            let one_state = list.geometries.len() <= 1 && list.instances.len() <= 1 && list.binds.len() <= 1;
            if !one_state {
                return SpikeCreated::Err { error: "an indirect list must share one geometry/instances/texture".into() };
            }
            let offsets = list.draws.iter().any(|draw| draw.first_instance != 0);
            if offsets && !gpu.device.features().contains(wgpu::Features::INDIRECT_FIRST_INSTANCE) {
                return SpikeCreated::Err {
                    error: "indirect draws with a non-zero first_instance need Features::INDIRECT_FIRST_INSTANCE"
                        .to_owned(),
                };
            }
            let bytes = indirect_args(&list.draws);
            let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spike draw-list indirect args"),
                size: bytes.len().max(INDIRECT_ARGS_BYTES) as u64,
                usage: wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.queue.write_buffer(&buffer, 0, &bytes);
            list.indirect = Some(buffer);
        }
        let id = self.allocate();
        self.lists.insert(id, list);
        add(&spike_probe::LIST_CREATE_NANOS, started);
        SpikeCreated::Ok { id }
    }

    pub fn patch_draw_list(
        &mut self,
        gpu: &RenderGpu,
        textures: &mut TextureRegistry,
        geometries: &mut GeometryRegistry,
        mail: &SpikePatchDrawList,
    ) {
        let started = Instant::now();
        let Some(list) = self.lists.get_mut(&mail.list_id) else {
            tracing::warn!(target: "aether_render", "spike patch_draw_list: unknown list id");
            return;
        };
        let first = mail.first as usize;
        if first + mail.draws.len() > list.draws.len() {
            tracing::warn!(target: "aether_render", "spike patch_draw_list: range past the list");
            return;
        }
        let pipeline = &self.pipelines[&list.pipeline_id];
        let mut ctx = ResolveCtx {
            gpu,
            textures,
            geometries,
            pipeline,
            instances: &self.instances,
            texture_binds: &mut self.texture_binds,
            shared: shared(&mut self.shared, &gpu.device),
        };
        // Resolve onto the tail (so the slot tables are shared), then move
        // the resolved entries over the patched range.
        let tail = list.draws.len();
        if let Err(error) = resolve(list, &mail.draws, &mut ctx) {
            list.draws.truncate(tail);
            tracing::warn!(target: "aether_render", %error, "spike patch_draw_list refused");
            return;
        }
        let patched: Vec<ResolvedDraw> = list.draws.drain(tail..).collect();
        if let Some(indirect) = &list.indirect {
            gpu.queue.write_buffer(indirect, (first * INDIRECT_ARGS_BYTES) as u64, &indirect_args(&patched));
        }
        list.draws[first..first + patched.len()].copy_from_slice(&patched);
        add(&spike_probe::LIST_PATCH_NANOS, started);
    }

    pub fn destroy_draw_list(&mut self, list_id: u32) {
        self.lists.remove(&list_id);
    }

    /// Record the passes whose target is (`frame` is `Some`) or is not
    /// the frame target.
    pub fn record(
        &mut self,
        gpu: &RenderGpu,
        encoder: &mut wgpu::CommandEncoder,
        textures: &mut TextureRegistry,
        geometries: &mut GeometryRegistry,
        passes: &[SpikeDrawPass],
        frame: Option<&Targets>,
    ) {
        for pass in passes {
            if (pass.target == SPIKE_FRAME_TARGET) != frame.is_some() {
                continue;
            }
            if let Err(error) = self.record_pass(gpu, encoder, textures, geometries, pass, frame) {
                spike_probe::DROPPED_PASSES.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(target: "aether_render", %error, "spike draw pass dropped");
            }
        }
    }

    fn record_pass(
        &mut self,
        gpu: &RenderGpu,
        encoder: &mut wgpu::CommandEncoder,
        textures: &mut TextureRegistry,
        geometries: &mut GeometryRegistry,
        mail: &SpikeDrawPass,
        frame: Option<&Targets>,
    ) -> Result<(), String> {
        let Self { shared: shared_slot, instances, arrays, pipelines, lists, texture_binds, attachments, scratch, .. } =
            self;
        let pipeline = pipelines.get(&mail.pipeline_id).ok_or("unknown pipeline id")?;
        if mail.uniforms.len() < pipeline.uniform_bytes {
            return Err("uniform bytes shorter than the pipeline's block".to_owned());
        }
        if pipeline.to_frame != frame.is_some() {
            return Err("pipeline target kind disagrees with the pass target".to_owned());
        }

        // The list: retained (already resolved) or resolved now.
        let started = Instant::now();
        let list: &ResolvedList = if mail.list_id != 0 {
            let list = lists.get(&mail.list_id).ok_or("unknown draw list id")?;
            if list.pipeline_id != mail.pipeline_id {
                return Err("draw list was validated against another pipeline".to_owned());
            }
            list
        } else {
            *scratch = ResolvedList::default();
            let unpacked: Vec<SpikeDraw>;
            let draws: &[SpikeDraw] = if mail.draw_bytes.is_empty() {
                &mail.draws
            } else {
                if mail.draw_bytes.len() % 32 != 0 {
                    return Err("draw bytes are not a multiple of 32".to_owned());
                }
                unpacked = mail
                    .draw_bytes
                    .chunks_exact(32)
                    .map(|chunk| {
                        let word = |at: usize| u32::from_le_bytes(chunk[at * 4..at * 4 + 4].try_into().expect("4 bytes"));
                        SpikeDraw {
                            geometry_id: word(0),
                            first_index: word(1),
                            index_count: word(2),
                            base_vertex: word(3),
                            instances_id: word(4),
                            first_instance: word(5),
                            instance_count: word(6),
                            texture_id: word(7),
                        }
                    })
                    .collect();
                &unpacked
            };
            let mut ctx = ResolveCtx {
                gpu,
                textures,
                geometries,
                pipeline,
                instances,
                texture_binds,
                shared: shared(shared_slot, &gpu.device),
            };
            resolve(scratch, draws, &mut ctx)?;
            scratch
        };
        add(&spike_probe::RESOLVE_NANOS, started);

        let array = match pipeline.textures {
            SpikeTextures::Array => Some(arrays.get(&mail.array_texture).ok_or("unknown texture array id")?),
            _ => None,
        };
        gpu.queue.write_buffer(&pipeline.uniform_buffer, 0, &mail.uniforms[..pipeline.uniform_bytes.max(4)]);

        let started = Instant::now();
        let (target_view, msaa_view);
        let (view, resolve_target, depth_view, store): (&wgpu::TextureView, _, _, _) = if let Some(frame) = frame {
            (frame.msaa_view(), None, pipeline.depth.then(|| frame.depth_view().clone()), wgpu::StoreOp::Store)
        } else {
            let entry = textures.entries.get_mut(&mail.target).ok_or("unknown target texture id")?;
            if entry.usage != TextureUsage::Writable {
                return Err("target texture is not writable".to_owned());
            }
            if wgpu_texture_format(entry.format) != pipeline.format {
                return Err("target texture format disagrees with the pipeline".to_owned());
            }
            entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
            let (width, height) = (entry.width, entry.height);
            target_view = entry
                .realized
                .as_ref()
                .expect("realized above")
                .texture()
                .create_view(&wgpu::TextureViewDescriptor::default());
            let depth = pipeline
                .depth
                .then(|| attachment(attachments, &gpu.device, (width, height, DEPTH_FORMAT, pipeline.samples)).clone());
            if pipeline.samples > 1 {
                msaa_view =
                    attachment(attachments, &gpu.device, (width, height, pipeline.format, pipeline.samples)).clone();
                (&msaa_view, Some(&target_view), depth, wgpu::StoreOp::Discard)
            } else {
                (&target_view, None, depth, wgpu::StoreOp::Store)
            }
        };
        let frame_target = frame.is_some();
        let clears = mail.clear && !frame_target;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spike draw-list pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: if clears {
                            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store,
                    },
                })],
                depth_stencil_attachment: depth_view.as_ref().map(|view| wgpu::RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: Some(wgpu::Operations {
                        load: if clears {
                            wgpu::LoadOp::Clear(1.0)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            encode(&mut pass, pipeline, list, array);
        }
        add(&spike_probe::ENCODE_NANOS, started);
        spike_probe::PASSES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
