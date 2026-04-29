// wgpu plumbing: owns the device/queue/surface plus a fixed vertex
// buffer, a camera uniform buffer + bind group, a shader module, and
// a render pipeline matching the (pos vec3, color vec3) vertex layout.
// Each frame the main thread hands in a byte blob drained from the
// render sink plus the latest camera view_proj; render() uploads
// both and issues one draw call. World-space vertices flow through
// the vertex shader's `camera.view_proj * vec4(position, 1.0)` to
// produce clip space.
//
// The surface holds a `'static` lifetime because it takes an owned
// `Arc<Window>`; the App owns the same Arc, so the window outlives
// the surface by construction.
//
// Offscreen target: every frame renders into an intermediate texture
// matching the surface's format and size (`OffscreenTarget`). The
// swapchain is updated by copying that texture into the current
// surface texture and presenting. If the surface is unavailable
// (occluded, lost, etc.), we still render to the offscreen and the
// capture path still works — we just skip the copy+present. This
// decouples "can we capture a frame" from "is the window visible",
// which matters for macOS where occluded windows stop delivering
// swapchain textures.
//
// Capture path: `copy_texture_to_buffer` reads from the offscreen
// texture (not the swapchain), so captures are independent of window
// state. Non-capture frames pay nothing beyond the one extra texture
// copy per frame.
//
// Depth: a `Depth32Float` texture paired with the offscreen target is
// cleared to 1.0 each frame and paired with `LessEqual` depth testing.
// Smaller clip-space z wins, so with the topdown camera (which looks
// down -Z) larger world z draws on top. Convention: floors / backdrop
// geometry at z=0, movers / foreground at z=0.1+.

use std::sync::Arc;

use winit::dpi::PhysicalSize;
use winit::window::Window;

const VERTEX_STRIDE: u64 = 24; // 3 * f32 position + 3 * f32 color
pub(crate) const VERTEX_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const CAMERA_UNIFORM_BYTES: u64 = 64; // 4x4 f32 column-major
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Wireframe-overlay shader: same vertex layout as `shader.wgsl` so the
/// pipeline shares the existing vertex buffer. The fragment stage emits
/// a flat dark color so wires read against any filled-color underneath.
const WIREFRAME_WGSL: &str = r#"
struct Camera {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0)
var<uniform> camera: Camera;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) color: vec3<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> @builtin(position) vec4<f32> {
    return camera.view_proj * vec4<f32>(in.position, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return vec4<f32>(0.05, 0.07, 0.12, 1.0);
}
"#;

/// Identity matrix in column-major order — what the camera uniform
/// holds before the first `aether.camera` mail arrives, so components
/// that still emit clip-space-ish world coordinates keep rendering.
pub const IDENTITY_VIEW_PROJ: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, //
];

/// 256-byte row-alignment required by wgpu's `copy_texture_to_buffer`.
/// Keeps padded-row math local to this module.
const COPY_ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

pub struct Gpu {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    /// Snapshot of the adapter chosen at `new()` — `AdapterInfo` plus
    /// the resolved `Limits`. Retained so `platform_info` can report
    /// what the substrate is running on without a second adapter
    /// request (which would be expensive and is a one-time fact
    /// anyway).
    pub adapter_info: wgpu::AdapterInfo,
    pub limits: wgpu::Limits,
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    /// Uniform buffer holding the current camera `view_proj` matrix
    /// (column-major, 64 bytes). Rewritten each frame from the shared
    /// `camera_state` before the draw pass — bytes come straight from
    /// whatever the `aether.camera` sink last captured, or identity
    /// until the first camera mail.
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    /// Intermediate render target. Everything draws here; the swapchain
    /// gets a `copy_texture_to_texture` blit + present. Sized to the
    /// surface, reallocated on resize.
    offscreen: OffscreenTarget,
    /// Depth target paired with `offscreen`. Cleared to 1.0 each frame;
    /// reallocated alongside the color target on resize. Same size +
    /// sample count as `offscreen`.
    depth: DepthTarget,
    /// Lazily-allocated readback buffer for the capture path. Sized
    /// to `padded_row_bytes * height`; reallocated on resize or first
    /// capture after a size change.
    readback: Option<Readback>,
    /// Wireframe overlay pipeline. `Some` only when `AETHER_WIREFRAME`
    /// is set to `1` / `overlay`. The main `pipeline` always draws
    /// first; this one then redraws the same vertex buffer in
    /// `PolygonMode::Line` over the top so geometry is legible.
    wire_pipeline: Option<wgpu::RenderPipeline>,
}

/// Wireframe rendering mode, set at boot via `AETHER_WIREFRAME`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WireframeMode {
    /// Filled faces only (default).
    Off,
    /// Lines only — the main pipeline runs in `PolygonMode::Line`.
    /// Useful when you want to see triangulation without face shading.
    Line,
    /// Filled faces with a wireframe overlay drawn on top.
    Overlay,
}

impl WireframeMode {
    fn from_env() -> Self {
        match std::env::var("AETHER_WIREFRAME").ok().as_deref() {
            None | Some("") | Some("0") | Some("off") => WireframeMode::Off,
            Some("line") => WireframeMode::Line,
            Some(_) => WireframeMode::Overlay, // "1", "overlay", etc.
        }
    }

    fn needs_polygon_mode_line(self) -> bool {
        !matches!(self, WireframeMode::Off)
    }
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
    pub fn new(window: Arc<Window>) -> Self {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(Arc::clone(&window))
            .expect("create_surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("no compatible wgpu adapter");
        let adapter_info = adapter.get_info();
        let limits = wgpu::Limits::default();

        // Wireframe rendering is opt-in via `AETHER_WIREFRAME`:
        //   unset / "0" / "off" → filled (default)
        //   "line"              → wireframe only
        //   "1" / "overlay"     → filled + wireframe overlay
        // The line modes need the adapter's `POLYGON_MODE_LINE`
        // feature (Metal supports it on modern macOS; some GLES-only
        // adapters don't). If unsupported we fall back to filled with
        // a warning rather than failing device creation.
        let mut wireframe_mode = WireframeMode::from_env();
        if wireframe_mode.needs_polygon_mode_line()
            && !adapter
                .features()
                .contains(wgpu::Features::POLYGON_MODE_LINE)
        {
            tracing::warn!(
                adapter = %adapter_info.name,
                "AETHER_WIREFRAME requested but adapter lacks POLYGON_MODE_LINE; falling back to filled"
            );
            wireframe_mode = WireframeMode::Off;
        }
        let required_features = if wireframe_mode.needs_polygon_mode_line() {
            wgpu::Features::POLYGON_MODE_LINE
        } else {
            wgpu::Features::empty()
        };

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("aether-substrate device"),
            required_features,
            required_limits: limits.clone(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::default(),
        }))
        .expect("request_device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        // Prefer sRGB so the clear color matches intuition.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            // COPY_DST: the swapchain receives a texture-to-texture
            // copy from the offscreen each frame. No draw pass
            // writes to it directly anymore.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let offscreen = create_offscreen(&device, format, config.width, config.height);
        let depth = create_depth(&device, config.width, config.height);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("hello-triangle shader"),
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
            label: Some("hello-triangle pipeline layout"),
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
            label: Some("hello-triangle pipeline"),
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
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: if wireframe_mode == WireframeMode::Line {
                    wgpu::PolygonMode::Line
                } else {
                    wgpu::PolygonMode::Fill
                },
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
            label: Some("hello-triangle vertex buffer"),
            size: VERTEX_BUFFER_BYTES as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Wireframe overlay pipeline: same vertex/uniform layout, but
        // `PolygonMode::Line` and a flat dark fragment color so the
        // wires read against any filled color underneath. A small
        // negative depth bias lifts the lines toward the camera so
        // they aren't z-fought by the filled triangles they trace.
        let wire_pipeline = if wireframe_mode == WireframeMode::Overlay {
            let wire_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("wireframe shader"),
                source: wgpu::ShaderSource::Wgsl(WIREFRAME_WGSL.into()),
            });
            Some(
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("wireframe overlay pipeline"),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &wire_shader,
                        entry_point: Some("vs_main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: std::slice::from_ref(&vertex_layout),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &wire_shader,
                        entry_point: Some("fs_main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: config.format,
                            blend: Some(wgpu::BlendState::REPLACE),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        strip_index_format: None,
                        front_face: wgpu::FrontFace::Ccw,
                        cull_mode: None,
                        polygon_mode: wgpu::PolygonMode::Line,
                        unclipped_depth: false,
                        conservative: false,
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: DEPTH_FORMAT,
                        depth_write_enabled: Some(false),
                        depth_compare: Some(wgpu::CompareFunction::LessEqual),
                        stencil: wgpu::StencilState::default(),
                        bias: wgpu::DepthBiasState {
                            constant: -1,
                            slope_scale: -1.0,
                            clamp: 0.0,
                        },
                    }),
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                }),
            )
        } else {
            None
        };

        Self {
            surface,
            device,
            queue,
            config,
            adapter_info,
            limits,
            pipeline,
            vertex_buffer,
            camera_buffer,
            camera_bind_group,
            offscreen,
            depth,
            readback: None,
            wire_pipeline,
        }
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.offscreen = create_offscreen(
            &self.device,
            self.config.format,
            self.config.width,
            self.config.height,
        );
        self.depth = create_depth(&self.device, self.config.width, self.config.height);
        // Invalidate the readback buffer — it's sized to the old
        // surface; the next capture reallocates.
        self.readback = None;
    }

    pub fn render(&mut self, vertices: &[u8], view_proj: &[f32; 16]) {
        let _ = self.render_impl(vertices, view_proj, false);
    }

    /// Variant of `render` that also copies the offscreen texture into
    /// a readback buffer, maps it, and returns an encoded PNG. On any
    /// capture-path failure, returns `Err(reason)`; the frame itself
    /// still renders and (if the surface is available) presents, since
    /// capture is a side channel.
    pub fn render_and_capture(
        &mut self,
        vertices: &[u8],
        view_proj: &[f32; 16],
    ) -> Result<Vec<u8>, String> {
        self.render_impl(vertices, view_proj, true)
            .ok_or_else(|| "capture did not produce a result".to_owned())?
    }

    /// Draw `vertices` into the offscreen target with `view_proj` as
    /// the camera uniform, optionally encode a capture copy, then
    /// best-effort blit to the swapchain and present. Returns
    /// `Some(Ok(png))` / `Some(Err(reason))` when `capture` is set;
    /// `None` when `capture` is false or the capture path couldn't
    /// allocate. Surface unavailability does *not* prevent capture —
    /// offscreen is the source of truth.
    fn render_impl(
        &mut self,
        vertices: &[u8],
        view_proj: &[f32; 16],
        capture: bool,
    ) -> Option<Result<Vec<u8>, String>> {
        let vertex_bytes = vertices.len();
        if vertex_bytes > VERTEX_BUFFER_BYTES {
            // Belt-and-suspenders: the render sink truncates at the cap
            // already, so we should never get here. Drop the frame
            // rather than overflow the buffer if a future caller
            // bypasses the sink-side clamp.
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
        // Camera uniform: upload the latest view_proj every frame.
        // 64 bytes is cheap enough that a dirty flag isn't worth it.
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
                if let Some(wire) = &self.wire_pipeline {
                    pass.set_pipeline(wire);
                    pass.draw(0..vertex_count, 0..1);
                }
            }
        }

        // Capture path: the copy runs against the offscreen texture,
        // which is unaffected by whether a swapchain image is available
        // this frame. That decouples capture from window visibility.
        let capture_meta = if capture {
            Some(self.prepare_capture_copy(&mut encoder))
        } else {
            None
        };

        // Try to obtain a swapchain texture for presentation. If the
        // surface is occluded/lost/outdated we just skip the blit +
        // present step — the offscreen is already fresh and captures
        // still resolve.
        let surface_tex = self.acquire_surface_texture();
        if let Some(tex) = surface_tex.as_ref() {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.offscreen.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &tex.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: self.offscreen.width,
                    height: self.offscreen.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        if let Some(tex) = surface_tex {
            tex.present();
        }

        capture_meta.map(|meta| self.finish_capture(meta))
    }

    /// Try to get the current swapchain texture. Reconfigures the
    /// surface on `Suboptimal`/`Lost`/`Outdated` so the next frame
    /// recovers; on anything else returns `None` and the caller skips
    /// the present step for this frame.
    fn acquire_surface_texture(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.config);
                Some(t)
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                None
            }
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => None,
            other => {
                tracing::warn!(
                    target: "aether_substrate::render",
                    status = ?other,
                    "surface.get_current_texture returned unexpected status",
                );
                None
            }
        }
    }

    /// Ensure a readback buffer exists sized to the offscreen texture,
    /// then encode a copy from the offscreen into it. Returns the
    /// padded row stride + dims so `finish_capture` can map the buffer
    /// and strip padding.
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
            format: self.config.format,
        }
    }

    fn finish_capture(&self, meta: CaptureMeta) -> Result<Vec<u8>, String> {
        let readback = self
            .readback
            .as_ref()
            .ok_or_else(|| "readback buffer missing".to_owned())?;

        // Map the buffer for read. wgpu's map_async is callback-based;
        // a simple poll+block yields the completion synchronously.
        let slice = readback.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        // Block the render thread until GPU finishes + map completes.
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| format!("device poll: {e:?}"))?;
        rx.recv()
            .map_err(|e| format!("map channel dropped: {e}"))?
            .map_err(|e| format!("buffer map failed: {e:?}"))?;

        let mapped = slice.get_mapped_range();
        // Strip row padding and (if the surface is BGRA) swizzle to
        // RGBA. The loop is one allocation: exactly `w*h*4` bytes.
        let mut rgba =
            Vec::with_capacity((meta.unpadded_row_bytes as usize) * (meta.height as usize));
        let swizzle_bgra = matches!(
            meta.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        for row in 0..meta.height {
            let start = (row * meta.padded_row_bytes) as usize;
            let end = start + meta.unpadded_row_bytes as usize;
            let src = &mapped[start..end];
            if swizzle_bgra {
                for chunk in src.chunks_exact(4) {
                    rgba.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
                }
            } else {
                rgba.extend_from_slice(src);
            }
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
        // RENDER_ATTACHMENT: the triangle pass writes here.
        // COPY_SRC: both the swapchain blit and the readback copy
        // read from this texture.
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

/// Dimensions + wire-format info captured at `copy_texture_to_buffer`
/// time and consumed after map. Lives on the render thread; not
/// Send — carries a `TextureFormat` (which is Copy anyway).
struct CaptureMeta {
    width: u32,
    height: u32,
    padded_row_bytes: u32,
    unpadded_row_bytes: u32,
    format: wgpu::TextureFormat,
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
