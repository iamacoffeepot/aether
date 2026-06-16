// Desktop wgpu plumbing. ADR-0071 phase C2: pipeline + targets moved
// into core's `RenderCapability` (via `RenderGpu` + `install_gpu`); this
// file now owns only the desktop-specific surface + swapchain config
// + optional wireframe overlay pipeline. Each frame creates an
// encoder, asks `render_handles.record_frame(...)` to record the
// shared offscreen pass, optionally records a capture copy, copies
// offscreen → swapchain, submits, and presents.
//
// Wireframe (`AETHER_WIREFRAME=line|overlay`) is desktop-only — a
// dev-affordance for inspecting triangulation on the windowed
// chassis. `Line` builds RenderGpu with `PolygonMode::Line` so the
// main pipeline draws as wires; `Overlay` keeps Fill and adds a
// second pipeline as an extra in `record_frame`.

use std::sync::Arc;

use aether_capabilities::{RenderGpu, RenderHandles};
use aether_kinds::{FrameCheck, FrameVerdict};
use aether_substrate::capture::ReferenceCapture;
use aether_substrate::render::{self, RenderError, encode_png, vertex_buffer_layout};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::visual;

pub use render::VERTEX_BUFFER_BYTES;
use std::env;
use std::iter;
use std::slice;

/// PNG bytes, optional [`FrameVerdict`], optional similarity score, and
/// optional similarity pass that `render_and_capture` produces
/// (iamacoffeepot/aether#1777, #1780). `verdict` is `Some` iff the
/// request carried `checks`; `similarity_score` / `similarity_pass` are
/// `Some` iff the request carried `similarity`.
type CaptureOutcome = Result<(Vec<u8>, Option<FrameVerdict>, Option<f32>, Option<bool>), String>;

/// Wireframe-overlay shader: same vertex layout as the main shader so
/// the pipeline shares the existing vertex buffer. The fragment stage
/// emits a flat dark color so wires read against any filled-color
/// underneath.
const WIREFRAME_WGSL: &str = r"
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
";

pub struct Gpu {
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    /// Snapshot of the adapter chosen at `new()` — `AdapterInfo` plus
    /// the resolved `Limits`. Retained so `platform_info` can report
    /// what the substrate is running on without a second adapter
    /// request (which would be expensive and is a one-time fact
    /// anyway).
    pub adapter_info: wgpu::AdapterInfo,
    pub limits: wgpu::Limits,
    /// Wireframe overlay pipeline. `Some` only when `AETHER_WIREFRAME`
    /// is `1` / `overlay`. `record_frame` draws this after the main
    /// pipeline as an extra inside the same render pass.
    wire_pipeline: Option<wgpu::RenderPipeline>,
    render_handles: RenderHandles,
    /// Submission index of the previous frame's `queue.submit`, drained
    /// at the top of the next `render_impl` to bound the window present
    /// loop to one frame in flight (iamacoffeepot/aether#1312).
    ///
    /// Without this bound the loop submits + presents as fast as
    /// `nextDrawable` backpressure allows (up to `maximumDrawableCount`
    /// = 3 frames of command buffers overlapping). Under sustained
    /// rendering that exposes a use-after-free in Metal/IOGPU's command-
    /// buffer completion path (`IOGPUMetalCommandBufferStorageReset` on
    /// `com.Metal.CompletionQueueDispatch`): the completion handler for
    /// an earlier frame tears its command buffer down while the main
    /// thread is acquiring/submitting a later one. Draining the prior
    /// submission first means that teardown runs while this thread is
    /// parked in `device.poll`, never racing the next submit.
    last_submission: Option<wgpu::SubmissionIndex>,
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
    // Process-level render debug toggle (AETHER_WIREFRAME), read at the desktop
    // GPU layer — not cap config.
    #[allow(clippy::disallowed_methods)]
    fn from_env() -> Self {
        match env::var("AETHER_WIREFRAME").ok().as_deref() {
            None | Some("" | "0" | "off") => Self::Off,
            Some("line") => Self::Line,
            Some(_) => Self::Overlay, // "1", "overlay", etc.
        }
    }

    fn needs_polygon_mode_line(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl Gpu {
    /// Construct the desktop chassis's wgpu state: instance, surface,
    /// adapter, device, queue, pipeline, and the shared `RenderHandles`
    /// install. Called once during desktop boot from inside winit's
    /// `resumed` handler.
    ///
    /// # Panics
    /// Panics if surface creation, adapter selection, or device
    /// acquisition fail — fail-fast per ADR-0063: the desktop chassis
    /// can't proceed without a usable GPU pipeline.
    // One-shot GPU pipeline setup: instance, surface, adapter, device,
    // shader, pipeline, depth, uniform — all tied together in a single
    // boot path. Splitting into helpers would force passing 6+
    // intermediate `wgpu` handles around without saving readability.
    //
    // `window` is owned because the boot path is a one-shot handoff:
    // the driver builds the `Arc<Window>` once and the GPU pipeline
    // takes a clone via `Arc::clone(&window)` for the surface; the
    // owning form mirrors the `RenderHandles` argument.
    #[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
    pub fn new(window: Arc<Window>, render_handles: RenderHandles) -> Self {
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

        let device = Arc::new(device);
        let queue = Arc::new(queue);

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        // Prefer sRGB so the clear color matches intuition.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
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

        let polygon_mode = if wireframe_mode == WireframeMode::Line {
            wgpu::PolygonMode::Line
        } else {
            wgpu::PolygonMode::Fill
        };
        render_handles.install_gpu(RenderGpu::new(
            Arc::clone(&device),
            Arc::clone(&queue),
            format,
            config.width,
            config.height,
            polygon_mode,
        ));

        // Wireframe overlay pipeline: same vertex/uniform layout, but
        // `PolygonMode::Line` and a flat dark fragment color so the
        // wires read against any filled color underneath. Built post-
        // install so it can borrow the bind group + pipeline layouts
        // from the installed RenderGpu's pipeline.
        let wire_pipeline = if wireframe_mode == WireframeMode::Overlay {
            let installed = render_handles.gpu().expect("install_gpu just succeeded");
            let pipeline_layout = &installed.pipeline.pipeline_layout;
            let wire_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("wireframe shader"),
                source: wgpu::ShaderSource::Wgsl(WIREFRAME_WGSL.into()),
            });
            let vertex_layout = vertex_buffer_layout();
            Some(
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("wireframe overlay pipeline"),
                    layout: Some(pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &wire_shader,
                        entry_point: Some("vs_main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: slice::from_ref(&vertex_layout),
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
            config,
            adapter_info,
            limits,
            wire_pipeline,
            render_handles,
            last_submission: None,
        }
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface
            .configure(&self.render_handles.device(), &self.config);
        self.render_handles
            .resize(self.config.width, self.config.height);
    }

    pub fn render(&mut self) {
        let _ = self.render_impl(None, None);
    }

    /// Variant of `render` that also copies the offscreen texture into
    /// a readback buffer, maps it, and returns an encoded PNG plus an
    /// optional [`FrameVerdict`] scored on the same raw RGBA (present
    /// iff `checks` is non-empty, iamacoffeepot/aether#1777) and an
    /// optional similarity score (present iff `reference` is `Some`,
    /// iamacoffeepot/aether#1780). On any capture-path failure, returns
    /// `Err(reason)`; the frame itself still renders and (if the surface
    /// is available) presents, since capture is a side channel.
    pub fn render_and_capture(
        &mut self,
        checks: &[FrameCheck],
        reference: Option<&ReferenceCapture>,
    ) -> CaptureOutcome {
        self.render_impl(Some(checks), reference)
            .ok_or_else(|| "capture did not produce a result".to_owned())?
    }

    /// Draw the current accumulator vertices into the offscreen target
    /// with the latest camera view-proj, optionally encode a capture
    /// copy, then best-effort blit to the swapchain and present.
    /// Returns `Some(Ok((png, verdict, score, pass)))` /
    /// `Some(Err(reason))` when `capture` is `Some(checks)`; `None`
    /// when capture wasn't requested or the capture path couldn't
    /// allocate. Surface unavailability does *not* prevent capture —
    /// offscreen is the source of truth.
    fn render_impl(
        &mut self,
        capture: Option<&[FrameCheck]>,
        reference: Option<&ReferenceCapture>,
    ) -> Option<CaptureOutcome> {
        let device = self.render_handles.device();
        let queue = self.render_handles.queue();

        // Bound the loop to one frame in flight (iamacoffeepot/aether#1312):
        // block until the previous frame's submission has fully completed
        // before recording the next. This drains the prior frame's command-
        // buffer completion (and its Metal/IOGPU teardown) while this thread
        // is parked here, so it can't race the acquire/submit below — and it
        // serialises writes to the shared persistent vertex/camera buffers
        // against the prior frame's reads. `poll` errors (a lost device)
        // fall through to `acquire_surface_texture`, which reconfigures.
        if let Some(index) = self.last_submission.take()
            && let Err(error) = device.poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: None,
            })
        {
            tracing::warn!(
                target: "aether_substrate::render",
                ?error,
                "device.poll for previous frame failed; continuing",
            );
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
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
        // Desktop renders every frame from current producer state —
        // commit-current semantic (false). The replay-cache mode is
        // reserved for `TestBench::capture` (iamacoffeepot/aether#847).
        match self
            .render_handles
            .record_frame(&mut encoder, extra_pipelines, false)
        {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => return None,
        }

        // ADR-0105 textured-quad overlay, recorded after the world pass —
        // commit-current semantic to match `record_frame` above.
        self.render_handles.record_overlay_pass(&mut encoder, false);

        // Capture path: the copy runs against the offscreen texture,
        // which is unaffected by whether a swapchain image is available
        // this frame. That decouples capture from window visibility.
        let capture_meta = if capture.is_some() {
            Some(self.render_handles.record_capture_copy(&mut encoder))
        } else {
            None
        };

        // Try to obtain a swapchain texture for presentation. If the
        // surface is occluded/lost/outdated we just skip the blit +
        // present step — the offscreen is already fresh and captures
        // still resolve.
        let surface_tex = self.acquire_surface_texture();
        if let Some(tex) = surface_tex.as_ref() {
            let (w, h) = self.render_handles.color_size();
            self.render_handles.with_color_texture(|src| {
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: src,
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
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
            });
        }

        // Retain the submission index so the next frame can wait on it
        // (see `last_submission`). The present command buffer wgpu creates
        // internally is committed after this submit and bounded by
        // `nextDrawable`, so it needs no separate tracking.
        self.last_submission = Some(queue.submit(iter::once(encoder.finish())));
        if let Some(tex) = surface_tex {
            tex.present();
        }

        // Map the readback once; encode the PNG, (when checks were
        // requested) score the verdict, and (when a reference was
        // provided) score the MAE similarity — all from the same
        // de-padded RGBA so every check sees the exact bytes the PNG
        // carries (iamacoffeepot/aether#1777, #1780).
        // `capture_meta` is `Some` iff `capture` is `Some`, so
        // `unwrap_or(&[])` only papers over an unreachable case.
        capture_meta.map(|meta| {
            let rgba = self.render_handles.map_capture_rgba(&meta)?;
            let png = encode_png(&rgba, meta.width, meta.height)?;

            // Score the similarity check before `run_checks` consumes
            // `rgba` (#1780). `score_similarity` clones the slice it needs.
            let (similarity_score, similarity_pass) =
                visual::score_similarity(&rgba, meta.width, meta.height, reference)?;

            let checks = capture.unwrap_or(&[]);
            let verdict = (!checks.is_empty())
                .then(|| visual::run_checks(rgba, meta.width, meta.height, checks));
            Ok((png, verdict, similarity_score, similarity_pass))
        })
    }

    /// Try to get the current swapchain texture. Reconfigures the
    /// surface on `Suboptimal`/`Lost`/`Outdated` so the next frame
    /// recovers; on anything else returns `None` and the caller skips
    /// the present step for this frame.
    fn acquire_surface_texture(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface
                    .configure(&self.render_handles.device(), &self.config);
                Some(t)
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface
                    .configure(&self.render_handles.device(), &self.config);
                None
            }
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => None,
            other @ wgpu::CurrentSurfaceTexture::Validation => {
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
