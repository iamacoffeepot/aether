// wgpu plumbing for the test-bench chassis. Owns the device/queue
// plus a fixed vertex buffer, a camera uniform buffer + bind group,
// a shader module, and a render pipeline matching the (pos vec3,
// color vec3) vertex layout. Each frame the main thread hands in a
// byte blob drained from the render sink plus the latest camera
// view_proj; render() uploads both and issues one draw call.
// World-space vertices flow through the vertex shader's
// `camera.view_proj * vec4(position, 1.0)` to produce clip space.
//
// This is the desktop chassis's render path with the presentation
// surface and swapchain blit removed (ADR-0067). There is no winit
// window, so we initialise wgpu without a `Surface`, render into an
// offscreen texture every frame, and the capture path reads from
// the same offscreen — exactly the same source desktop's capture
// uses, just without the parallel swapchain copy.
//
// Format is fixed to `Rgba8UnormSrgb` rather than queried from a
// surface — there's no surface to ask, and committing to RGBA at
// init time means the readback path doesn't need a BGRA swizzle.
//
// Depth: a `Depth32Float` texture paired with the offscreen target
// is cleared to 1.0 each frame and paired with `LessEqual` depth
// testing. Same convention as desktop — floors / backdrop geometry
// at z=0, movers / foreground at z=0.1+.

const VERTEX_STRIDE: u64 = 24; // 3 * f32 position + 3 * f32 color
pub(crate) const VERTEX_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const CAMERA_UNIFORM_BYTES: u64 = 64; // 4x4 f32 column-major
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Render target format. Fixed to RGBA so the capture-path readback
/// can extend straight into its output buffer with no swizzle.
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Identity matrix in column-major order — what the camera uniform
/// holds before the first `aether.camera` mail arrives.
pub const IDENTITY_VIEW_PROJ: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, //
];

/// 256-byte row-alignment required by wgpu's `copy_texture_to_buffer`.
const COPY_ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Snapshot of the adapter chosen at `new()`. Kept for
    /// diagnostics — desktop also exposes this for `platform_info`,
    /// which test-bench replies `Err` to (window-only operation), but
    /// the field is cheap and lets the boot log report adapter info.
    pub adapter_info: wgpu::AdapterInfo,
    /// Resolved adapter limits. Kept for diagnostics; desktop uses
    /// the equivalent for `platform_info` which test-bench replies
    /// `Err` to.
    #[allow(dead_code)]
    pub limits: wgpu::Limits,
    width: u32,
    height: u32,
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    offscreen: OffscreenTarget,
    depth: DepthTarget,
    /// Lazily-allocated readback buffer for the capture path.
    /// Reallocated on resize or first capture after a size change.
    readback: Option<Readback>,
}

struct OffscreenTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

struct DepthTarget {
    #[allow(dead_code)] // keep the texture alive; only the view is bound
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

struct Readback {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
}

impl Gpu {
    /// Initialise wgpu with no presentation surface. `width` and
    /// `height` size the offscreen color + depth targets — they're
    /// the dimensions every captured frame will report. wgpu picks
    /// an adapter via `request_adapter` with `compatible_surface:
    /// None`; on CI runners this resolves to whatever Vulkan/Metal/
    /// DX12 driver is available (ADR-0067 driver fallback policy).
    pub fn new(width: u32, height: u32) -> Self {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no compatible wgpu adapter");
        let adapter_info = adapter.get_info();
        let limits = wgpu::Limits::default();

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("aether-test-bench device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits.clone(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::default(),
        }))
        .expect("request_device");

        let width = width.max(1);
        let height = height.max(1);

        let offscreen = create_offscreen(&device, COLOR_FORMAT, width, height);
        let depth = create_depth(&device, width, height);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test-bench shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let camera_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("camera bind group layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(CAMERA_UNIFORM_BYTES),
                    },
                    count: None,
                }],
            });

        let camera_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera uniform"),
            size: CAMERA_UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&camera_buffer, 0, bytemuck::cast_slice(&IDENTITY_VIEW_PROJ));

        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera bind group"),
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("test-bench pipeline layout"),
            bind_group_layouts: &[Some(&camera_bind_group_layout)],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: VERTEX_STRIDE,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    offset: 12,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x3,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test-bench pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: std::slice::from_ref(&vertex_layout),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: COLOR_FORMAT,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
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
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test-bench vertex buffer"),
            size: VERTEX_BUFFER_BYTES as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            device,
            queue,
            adapter_info,
            limits,
            width,
            height,
            pipeline,
            vertex_buffer,
            camera_buffer,
            camera_bind_group,
            offscreen,
            depth,
            readback: None,
        }
    }

    /// Resize the offscreen target. Test-bench has no surface, so a
    /// resize just reallocates the offscreen color + depth textures
    /// and invalidates the readback buffer. Scenario scripts that need
    /// a different aspect ratio can mail an advance-style control to
    /// trigger this — for v1 the size is fixed at boot.
    #[allow(dead_code)] // wired in PR2 alongside test_bench.advance kinds
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        self.offscreen = create_offscreen(&self.device, COLOR_FORMAT, width, height);
        self.depth = create_depth(&self.device, width, height);
        self.readback = None;
    }

    pub fn render(&mut self, vertices: &[u8], view_proj: &[f32; 16]) {
        let _ = self.render_impl(vertices, view_proj, false);
    }

    /// Variant of `render` that also copies the offscreen texture
    /// into a readback buffer, maps it, and returns an encoded PNG.
    /// On any capture-path failure, returns `Err(reason)`; the frame
    /// still rendered to the offscreen — capture is a side channel.
    pub fn render_and_capture(
        &mut self,
        vertices: &[u8],
        view_proj: &[f32; 16],
    ) -> Result<Vec<u8>, String> {
        self.render_impl(vertices, view_proj, true)
            .ok_or_else(|| "capture did not produce a result".to_owned())?
    }

    /// Draw `vertices` into the offscreen target with `view_proj` as
    /// the camera uniform, optionally encode a capture copy. Returns
    /// `Some(Ok(png))` / `Some(Err(reason))` when `capture` is set;
    /// `None` when `capture` is false or the capture path couldn't
    /// allocate. There is no presentation step — desktop's swapchain
    /// blit is omitted because there's no surface.
    fn render_impl(
        &mut self,
        vertices: &[u8],
        view_proj: &[f32; 16],
        capture: bool,
    ) -> Option<Result<Vec<u8>, String>> {
        let vertex_bytes = vertices.len();
        if vertex_bytes > VERTEX_BUFFER_BYTES {
            tracing::warn!(
                target: "aether_substrate::render",
                vertex_bytes,
                cap = VERTEX_BUFFER_BYTES,
                "dropping frame: vertex bytes exceed fixed buffer",
            );
            return None;
        }
        if !vertices.is_empty() {
            self.queue.write_buffer(&self.vertex_buffer, 0, vertices);
        }
        self.queue
            .write_buffer(&self.camera_buffer, 0, bytemuck::cast_slice(view_proj));
        let vertex_count = (vertex_bytes as u64 / VERTEX_STRIDE) as u32;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("triangle pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.offscreen.view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.07,
                            b: 0.12,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth.view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if vertex_count > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.camera_bind_group, &[]);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..vertex_bytes as u64));
                pass.draw(0..vertex_count, 0..1);
            }
        }

        let capture_meta = if capture {
            Some(self.prepare_capture_copy(&mut encoder))
        } else {
            None
        };

        self.queue.submit(std::iter::once(encoder.finish()));

        capture_meta.map(|meta| self.finish_capture(meta))
    }

    /// Ensure a readback buffer exists sized to the offscreen
    /// texture, then encode a copy from the offscreen into it.
    /// Returns the padded row stride + dims so `finish_capture` can
    /// map the buffer and strip padding.
    fn prepare_capture_copy(&mut self, encoder: &mut wgpu::CommandEncoder) -> CaptureMeta {
        let width = self.offscreen.width;
        let height = self.offscreen.height;
        let bytes_per_pixel = 4u32;
        let unpadded_row_bytes = width * bytes_per_pixel;
        let padded_row_bytes = align_up(unpadded_row_bytes, COPY_ROW_ALIGN);
        let buffer_size = u64::from(padded_row_bytes) * u64::from(height);

        let needs_realloc = match &self.readback {
            Some(rb) => rb.width != width || rb.height != height,
            None => true,
        };
        if needs_realloc {
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("capture readback"),
                size: buffer_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            self.readback = Some(Readback {
                buffer,
                width,
                height,
            });
        }

        let readback = self.readback.as_ref().expect("readback just set");

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.offscreen.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_bytes),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        CaptureMeta {
            width,
            height,
            padded_row_bytes,
            unpadded_row_bytes,
        }
    }

    fn finish_capture(&self, meta: CaptureMeta) -> Result<Vec<u8>, String> {
        let readback = self
            .readback
            .as_ref()
            .ok_or_else(|| "readback buffer missing".to_owned())?;

        let slice = readback.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| format!("device poll: {e:?}"))?;
        rx.recv()
            .map_err(|e| format!("map channel dropped: {e}"))?
            .map_err(|e| format!("buffer map failed: {e:?}"))?;

        let mapped = slice.get_mapped_range();
        // Strip row padding. Format is fixed RGBA so no swizzle is
        // needed (desktop's path BGRA-swizzles when the surface
        // picked Bgra8Unorm; test-bench commits to RGBA at init).
        let mut rgba =
            Vec::with_capacity((meta.unpadded_row_bytes as usize) * (meta.height as usize));
        for row in 0..meta.height {
            let start = (row * meta.padded_row_bytes) as usize;
            let end = start + meta.unpadded_row_bytes as usize;
            rgba.extend_from_slice(&mapped[start..end]);
        }
        drop(mapped);
        readback.buffer.unmap();

        encode_png(&rgba, meta.width, meta.height)
    }
}

fn create_offscreen(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> OffscreenTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen color target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    OffscreenTarget {
        texture,
        view,
        width,
        height,
    }
}

fn create_depth(device: &wgpu::Device, width: u32, height: u32) -> DepthTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    DepthTarget { texture, view }
}

struct CaptureMeta {
    width: u32,
    height: u32,
    padded_row_bytes: u32,
    unpadded_row_bytes: u32,
}

fn align_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png header: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("png write: {e}"))?;
    }
    Ok(out)
}
