// wgpu plumbing for the desktop chassis. Owns the device/queue/surface
// plus the shared `core::render::Pipeline` + `Targets` (issue 421);
// each frame the main thread hands in a byte blob drained from the
// render sink plus the latest camera view_proj. The render path goes
// through `core::render::record_main_pass` which uploads + draws into
// the offscreen target; we then optionally blit the offscreen into
// the swapchain and present.
//
// Capture path: `prepare_capture_copy` reads from the offscreen
// texture (not the swapchain), so captures are independent of window
// state. Non-capture frames pay nothing beyond the one extra texture
// copy per frame.
//
// Wireframe (`AETHER_WIREFRAME=line|overlay`) is desktop-only — a
// dev-affordance for inspecting triangulation on the windowed
// chassis. `Line` re-builds the main pipeline with `PolygonMode::Line`;
// `Overlay` keeps filled rendering and adds a second draw inside the
// same pass via `record_main_pass`'s `extra_pipelines` slice.

use std::sync::Arc;

use aether_substrate_core::render::{
    self, CaptureMeta, Pipeline, RenderError, Targets, build_main_pipeline, finish_capture,
    prepare_capture_copy, record_main_pass, vertex_buffer_layout,
};
use winit::dpi::PhysicalSize;
use winit::window::Window;

pub use render::VERTEX_BUFFER_BYTES;

/// Wireframe-overlay shader: same vertex layout as the main shader so
/// the pipeline shares the existing vertex buffer. The fragment stage
/// emits a flat dark color so wires read against any filled-color
/// underneath.
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
    pipeline: Pipeline,
    targets: Targets,
    /// Wireframe overlay pipeline. `Some` only when `AETHER_WIREFRAME`
    /// is set to `1` / `overlay`. The main pipeline always draws
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

        let targets = Targets::new(&device, format, config.width, config.height);
        let polygon_mode = if wireframe_mode == WireframeMode::Line {
            wgpu::PolygonMode::Line
        } else {
            wgpu::PolygonMode::Fill
        };
        let pipeline = build_main_pipeline(&device, &queue, format, polygon_mode);

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
            let vertex_layout = vertex_buffer_layout();
            Some(
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("wireframe overlay pipeline"),
                    layout: Some(&pipeline.pipeline_layout),
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
                        format: render::DEPTH_FORMAT,
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
            targets,
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
        self.targets
            .resize(&self.device, self.config.width, self.config.height);
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
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame encoder"),
            });

        let wire_ref = self.wire_pipeline.as_ref();
        let extras_storage: [&wgpu::RenderPipeline; 1];
        let extra_pipelines: &[&wgpu::RenderPipeline] = match wire_ref {
            Some(p) => {
                extras_storage = [p];
                &extras_storage
            }
            None => &[],
        };
        match record_main_pass(
            &self.queue,
            &mut encoder,
            &self.pipeline,
            &self.targets,
            vertices,
            view_proj,
            extra_pipelines,
        ) {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => return None,
        }

        // Capture path: the copy runs against the offscreen texture,
        // which is unaffected by whether a swapchain image is available
        // this frame. That decouples capture from window visibility.
        let capture_meta: Option<CaptureMeta> = if capture {
            Some(prepare_capture_copy(
                &self.device,
                &mut self.targets,
                &mut encoder,
            ))
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
                    texture: self.targets.color_texture(),
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
                    width: self.targets.width(),
                    height: self.targets.height(),
                    depth_or_array_layers: 1,
                },
            );
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        if let Some(tex) = surface_tex {
            tex.present();
        }

        capture_meta.map(|meta| finish_capture(&self.device, &self.targets, &meta))
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
}
