// wgpu plumbing for milestone 3b: owns the device/queue/surface as in
// 3a, plus a fixed vertex buffer, a shader module, and a render
// pipeline matching the (pos vec2, color vec3) vertex layout. Each
// frame the main thread hands in a byte blob drained from the render
// sink; render() uploads it and issues one draw call.
//
// The surface holds a `'static` lifetime because it takes an owned
// `Arc<Window>`; the App owns the same Arc, so the window outlives
// the surface by construction.
//
// Capture path: the surface is configured with COPY_SRC so the
// swapchain texture can be copied into a readback buffer in the same
// encoder as the frame. When a capture is requested, render() encodes
// the copy alongside the normal pass, submits, maps the buffer, and
// returns PNG bytes. Non-capture frames pay nothing beyond the one
// extra usage bit on the surface.

use std::sync::Arc;

use winit::dpi::PhysicalSize;
use winit::window::Window;

const VERTEX_STRIDE: u64 = 20; // 2 * f32 position + 3 * f32 color
const VERTEX_BUFFER_BYTES: u64 = 64 * 1024;

/// 256-byte row-alignment required by wgpu's `copy_texture_to_buffer`.
/// Keeps padded-row math local to this module.
const COPY_ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

pub struct Gpu {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    /// Lazily-allocated readback buffer for the capture path. Sized
    /// to `padded_row_bytes * height`; reallocated on resize or first
    /// capture after a size change.
    readback: Option<Readback>,
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
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("aether-substrate device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
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
            // COPY_SRC: swapchain texture is read by the capture path.
            // Supported on every desktop wgpu backend; platforms that
            // reject it will fail surface.configure and we'd fall back
            // to an intermediate target — not needed today.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("hello-triangle shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("hello-triangle pipeline layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: VERTEX_STRIDE,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 8,
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
                buffers: &[vertex_layout],
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
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("hello-triangle vertex buffer"),
            size: VERTEX_BUFFER_BYTES,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            vertex_buffer,
            readback: None,
        }
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        // Invalidate the readback buffer — it's sized to the old
        // surface; the next capture reallocates.
        self.readback = None;
    }

    pub fn render(&mut self, vertices: &[u8]) {
        self.render_impl(vertices, false);
    }

    /// Variant of `render` that also copies the swapchain texture into
    /// a readback buffer, maps it, and returns an encoded PNG. On any
    /// capture-path failure, returns `Err(reason)`; the frame itself
    /// still renders and presents (capture is side-channel).
    pub fn render_and_capture(&mut self, vertices: &[u8]) -> Result<Vec<u8>, String> {
        self.render_impl(vertices, true)
            .ok_or_else(|| "capture skipped — no surface texture this frame".to_owned())?
    }

    fn render_impl(&mut self, vertices: &[u8], capture: bool) -> Option<Result<Vec<u8>, String>> {
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.config);
                t
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return None;
            }
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => {
                return None;
            }
            other => {
                tracing::warn!(
                    target: "aether_substrate::render",
                    status = ?other,
                    "surface.get_current_texture returned unexpected status",
                );
                return None;
            }
        };

        let vertex_bytes = vertices.len() as u64;
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
        let vertex_count = (vertex_bytes / VERTEX_STRIDE) as u32;

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("triangle pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if vertex_count > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..vertex_bytes));
                pass.draw(0..vertex_count, 0..1);
            }
        }

        // Capture path: issue a texture→buffer copy in the same
        // encoder, submit everything, then present so the window
        // update isn't delayed by the readback. The buffer map + PNG
        // encode happens after present.
        let capture_meta = if capture {
            Some(self.prepare_capture_copy(&output.texture, &mut encoder))
        } else {
            None
        };

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        capture_meta.map(|meta| self.finish_capture(meta))
    }

    /// Ensure a readback buffer exists sized to the current surface,
    /// then encode a copy from the swapchain texture into it. Returns
    /// the padded row stride + dims so `finish_capture` can map the
    /// buffer and strip padding.
    fn prepare_capture_copy(
        &mut self,
        texture: &wgpu::Texture,
        encoder: &mut wgpu::CommandEncoder,
    ) -> CaptureMeta {
        let width = self.config.width;
        let height = self.config.height;
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
                texture,
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
