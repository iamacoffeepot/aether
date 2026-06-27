//! Driver-facing GPU bundle ([`RenderGpu`]) and accumulator state
//! ([`RenderHandles`]) for the `aether.render` cap. Shared between the
//! cap's dispatcher thread (write side for accumulators) and the
//! chassis driver (read side for accumulators, install + read for GPU).
//! All accumulator fields are `Arc`s so cloning is cheap and shutdown
//! drops are independent.

// Frame-vertex / last-submitted Mutex guards are held through the
// per-frame swap and append sequence on purpose — the swap and
// subsequent length math read the buffer's current state and write
// back; releasing the guard mid-sequence opens a TOCTOU window
// where a sibling tick's producer mutates the buffer in between.
#![allow(clippy::significant_drop_tightening)]

use std::mem;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};

use aether_kinds::{QuadScale, QuadSpace};
use aether_substrate::render::{
    CaptureMeta, OverlayDraw, Pipeline, QUAD_VERTEX_STRIDE, QUAD_VERTICES_PER_QUAD, QuadPipeline,
    RenderError, Targets, build_main_pipeline, build_quad_pipeline, finish_capture,
    map_capture_rgba, prepare_capture_copy, push_screen_quad_vertices, push_world_quad_vertices,
    record_main_pass, record_quad_overlay_pass,
};

use super::quad::QuadBatch;
use super::texture::TextureRegistry;

/// Bundle of accumulator state plus GPU resources, shared between
/// the cap's dispatcher thread (write side for accumulators) and the
/// chassis driver (read side for accumulators, install + read for
/// GPU). All fields are `Arc`s so cloning is cheap and shutdown
/// drops are independent.
#[derive(Clone)]
pub struct RenderHandles {
    /// Per-frame accumulator. `on_draw_triangle` appends bytes
    /// here; `record_frame` consumes by swapping with
    /// `last_submitted` and clearing.
    pub frame_vertices: Arc<Mutex<Vec<u8>>>,
    /// Most-recently-rendered geometry, kept across frames
    /// (iamacoffeepot/aether#847). When `record_frame` runs with
    /// an empty `frame_vertices` — typically a `TestBench::capture`
    /// that didn't dispatch a `Tick` — the GPU draw replays this
    /// buffer so the captured frame matches "what the user would
    /// see right now" instead of clear-color.
    ///
    /// Lock ordering: `frame_vertices` first, then `last_submitted`
    /// when both are held. Today only `record_frame` holds both;
    /// callers reading `last_submitted` in isolation are fine.
    pub last_submitted: Arc<Mutex<Vec<u8>>>,
    pub triangles_rendered: Arc<AtomicU64>,
    pub camera_state: Arc<Mutex<[f32; 16]>>,
    /// Per-frame textured-quad accumulator (ADR-0105). `on_draw_
    /// textured_quads` pushes a [`QuadBatch`] here; `record_overlay_
    /// pass` consumes by swapping with `quad_last_submitted` — the
    /// same immediate-mode cache the triangle path uses, so a
    /// `TestBench::capture` replays the last committed quads.
    pub(in crate::render) quad_frame: Arc<Mutex<Vec<QuadBatch>>>,
    /// Most-recently-rendered quad batches, kept across frames so an
    /// idle `capture` (no producer this frame) replays them, matching
    /// `last_submitted`'s role for triangles.
    pub(in crate::render) quad_last_submitted: Arc<Mutex<Vec<QuadBatch>>>,
    /// Session-scoped texture registry: staged CPU pixels + lazily-
    /// realized GPU textures. Written by the cap dispatcher thread
    /// (`create_texture` / `update_texture`), realized + read by the
    /// driver thread at record time.
    pub(in crate::render) textures: Arc<Mutex<TextureRegistry>>,
    /// wgpu state, installed post-cap-construction by the driver via
    /// [`Self::install_gpu`]. Boots empty because winit 0.30's
    /// `ActiveEventLoop::create_window` only fires inside `resumed`,
    /// after `Builder::build` has returned. Test-bench (no surface)
    /// installs immediately after `build_passive`; desktop installs
    /// in its `resumed` handler. Encoder-level methods panic if
    /// called before install — in practice every code path that
    /// calls them runs after the install site.
    pub(in crate::render) gpu: Arc<OnceLock<RenderGpu>>,
}

/// Commit a frame's live accumulator into its cache, the shared
/// swap-or-clear both the triangle (`frame_vertices`) and quad
/// (`quad_frame`) passes run before recording (iamacoffeepot/aether#847).
/// Locks `live` then `last` — the documented lock ordering — and holds
/// both across the swap so a sibling tick can't mutate `live` mid-commit.
///
/// - `live` non-empty: the producer emitted this frame, so swap it into
///   `last` and clear `live` (preserving its capacity) for the next tick.
/// - `live` empty, `replay_cache_when_idle == false`: commit-current —
///   clear `last` so the next frame reflects "the producer chose not to
///   emit."
/// - `live` empty, `replay_cache_when_idle == true`: leave `last` alone
///   so a subsequent record replays its current contents.
fn commit_or_replay<T>(live: &Mutex<Vec<T>>, last: &Mutex<Vec<T>>, replay_cache_when_idle: bool) {
    let mut live = live.lock().expect("mutex poisoned; fail-fast per ADR-0063");
    let mut last = last.lock().expect("mutex poisoned; fail-fast per ADR-0063");
    if !live.is_empty() {
        mem::swap(&mut *live, &mut *last);
        live.clear();
    } else if !replay_cache_when_idle {
        last.clear();
    }
}

impl RenderHandles {
    /// Install the wgpu resources the encoder-level methods read.
    /// The driver constructs [`RenderGpu`] once it has a device +
    /// queue — for desktop that's inside `resumed` after winit hands
    /// back a window and surface; for test-bench it's right after
    /// `build_passive` returns.
    ///
    /// # Panics
    /// Panics if called more than once — fail-fast per ADR-0063:
    /// install is the chassis's promise that wgpu state is now
    /// ready and stable for the chassis lifetime; a double install
    /// indicates a chassis-wiring bug.
    pub fn install_gpu(&self, gpu: RenderGpu) {
        self.gpu
            .set(gpu)
            .ok()
            .expect("RenderHandles::install_gpu called twice");
    }

    /// Returns the installed [`RenderGpu`], or `None` if `install_gpu`
    /// hasn't been called yet. Chassis-side glue that needs raw
    /// access to the pipeline's bind group layouts (e.g. desktop's
    /// wireframe overlay pipeline construction) reaches in here.
    #[must_use]
    pub fn gpu(&self) -> Option<&RenderGpu> {
        self.gpu.get()
    }

    fn expect_gpu(&self) -> &RenderGpu {
        self.gpu.get().expect(
            "RenderHandles::install_gpu must be called before encoder-level methods. \
         Desktop installs in winit's resumed; test-bench installs after build_passive.",
        )
    }

    /// Read the latest camera view-proj and record the main render
    /// pass into `encoder` against the current frame's geometry.
    /// `extra_pipelines` are drawn after the main pipeline inside
    /// the same render pass — desktop passes a wireframe overlay
    /// pipeline here when `AETHER_WIREFRAME=overlay`; test-bench
    /// passes `&[]`.
    ///
    /// ## Cache semantics (iamacoffeepot/aether#847)
    ///
    /// If `frame_vertices` holds new emissions from this tick's
    /// `on_draw_triangle` calls, swap them into `last_submitted`
    /// and clear the live accumulator (the swapped-out buffer,
    /// now in `live`, becomes the next tick's staging area). The
    /// render pass then draws from `last_submitted`.
    ///
    /// If `frame_vertices` is empty, `replay_cache_when_idle`
    /// picks the behaviour:
    ///
    /// - `false` — **commit-current**: clear `last_submitted` so
    ///   the next frame reflects "the producer chose not to
    ///   emit," and render an empty draw list (clear-color
    ///   frame). Used by desktop's per-frame draw and by the
    ///   test-bench's advance path. Matches a game's normal
    ///   semantic: if the producer stops drawing, the screen
    ///   goes to clear color.
    /// - `true` — **replay-cache**: leave `last_submitted`
    ///   untouched and render its current contents. Used by
    ///   `TestBench::capture` when it didn't dispatch a `Tick`
    ///   of its own — the cache holds whatever the last advance
    ///   committed, which is the right "what would the user
    ///   see right now" answer. Retires the historical
    ///   `nudge_tick` boilerplate.
    ///
    /// Lock ordering: `frame_vertices` first, then
    /// `last_submitted`. Today only this function holds both.
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called, or if any of the
    /// internal mutexes (frame vertices, last submitted, camera
    /// state, targets) are poisoned — fail-fast per ADR-0063: both
    /// indicate a substrate-level invariant violation.
    pub fn record_frame(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        extra_pipelines: &[&wgpu::RenderPipeline],
        replay_cache_when_idle: bool,
    ) -> Result<(), RenderError> {
        let gpu = self.expect_gpu();
        commit_or_replay(
            &self.frame_vertices,
            &self.last_submitted,
            replay_cache_when_idle,
        );
        let view_proj = *self
            .camera_state
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        let last = self
            .last_submitted
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        let targets = gpu
            .targets
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        record_main_pass(
            &gpu.queue,
            encoder,
            &gpu.pipeline,
            &targets,
            &last,
            &view_proj,
            extra_pipelines,
        )
    }

    /// Record the textured-quad overlay pass (ADR-0105) into `encoder`
    /// after [`Self::record_frame`] — a sibling pass that draws the
    /// accumulated `Screen`-space quads over the world geometry with
    /// alpha blending and no depth.
    ///
    /// `replay_cache_when_idle` mirrors [`Self::record_frame`]'s cache
    /// semantics for quads: an empty live accumulator commits-current
    /// (clears the cache) under `false` — the per-frame draw / advance
    /// path — and replays the cache under `true` — `TestBench::capture`
    /// without a dispatched tick.
    ///
    /// Each batch realizes its texture lazily (creating the wgpu
    /// texture + bind group on first use, re-uploading on a dirtied
    /// staging buffer), expands its quads into vertices, and draws
    /// with that texture's bind group. An unknown `texture_id`
    /// warn-drops the batch. `World`-space quads transform their
    /// anchor through the latest `view_proj` (ADR-0105).
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called, or if any internal
    /// mutex is poisoned — fail-fast per ADR-0063.
    // Two-pass texture realization + quad expansion in a single
    // function avoids threading split borrows through multiple
    // helpers; the line count reflects the World/Screen branching
    // added in #1699.
    #[allow(clippy::too_many_lines)]
    pub fn record_overlay_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        replay_cache_when_idle: bool,
    ) {
        let gpu = self.expect_gpu();
        commit_or_replay(
            &self.quad_frame,
            &self.quad_last_submitted,
            replay_cache_when_idle,
        );
        let batches = self
            .quad_last_submitted
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063")
            .clone();
        if batches.is_empty() {
            return;
        }

        let targets = gpu
            .targets
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        #[allow(clippy::cast_precision_loss)]
        let viewport = [targets.width() as f32, targets.height() as f32];

        let view_proj = *self
            .camera_state
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");

        let mut registry = self
            .textures
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");

        // First pass: realize / re-upload every texture the frame
        // references (Screen and World batches share the same atlas),
        // mutably borrowing the registry.
        for batch in &batches {
            if let Some(entry) = registry.entries.get_mut(&batch.texture_id) {
                entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.quad_pipeline);
            } else {
                tracing::warn!(
                    target: "aether_capabilities::render",
                    texture_id = batch.texture_id,
                    "draw_textured_quads for unknown texture id; dropping the batch",
                );
            }
        }

        // Second pass: expand quads into vertices and build the draw
        // list, immutably borrowing each realized texture's bind group.
        let mut vertex_bytes = Vec::new();
        let mut draws: Vec<OverlayDraw<'_>> = Vec::new();
        for batch in &batches {
            let Some(entry) = registry.entries.get(&batch.texture_id) else {
                continue;
            };
            let Some(realized) = entry.realized.as_ref() else {
                continue;
            };
            #[allow(clippy::cast_possible_truncation)]
            let first_vertex = (vertex_bytes.len() / QUAD_VERTEX_STRIDE as usize) as u32;
            match &batch.space {
                QuadSpace::Screen => {
                    for quad in &batch.quads {
                        push_screen_quad_vertices(
                            &mut vertex_bytes,
                            [quad.x, quad.y, quad.width, quad.height],
                            [quad.u0, quad.v0, quad.u1, quad.v1],
                            quad.tint,
                        );
                    }
                }
                QuadSpace::World { anchor, scale } => {
                    // k < 0 => Pixels mode (shader uses clip.w for
                    // constant on-screen size). k > 0 => Distance mode
                    // (constant k, label shrinks with depth; holds its
                    // size at reference_distance).
                    let k = match scale {
                        QuadScale::Pixels => -1.0_f32,
                        QuadScale::Distance { reference_distance } => *reference_distance,
                    };
                    for quad in &batch.quads {
                        push_world_quad_vertices(
                            &mut vertex_bytes,
                            *anchor,
                            [quad.x, quad.y, quad.width, quad.height],
                            [quad.u0, quad.v0, quad.u1, quad.v1],
                            quad.tint,
                            k,
                        );
                    }
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            let vertex_count = (batch.quads.len() * QUAD_VERTICES_PER_QUAD) as u32;
            if vertex_count == 0 {
                continue;
            }
            draws.push(OverlayDraw {
                bind_group: realized.bind_group(),
                first_vertex,
                vertex_count,
            });
        }

        record_quad_overlay_pass(
            &gpu.queue,
            encoder,
            &gpu.quad_pipeline,
            &targets,
            &vertex_bytes,
            &draws,
            viewport,
            view_proj,
        );
    }

    /// Encode a copy of the offscreen color target into a readback
    /// buffer. Pair with [`Self::finish_capture`] after submit. The
    /// readback buffer is reallocated on size mismatch with the
    /// current offscreen, so any sequence of resize → `record_frame` →
    /// `record_capture_copy` → submit → `finish_capture` works.
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called or if the targets
    /// mutex is poisoned — fail-fast per ADR-0063.
    pub fn record_capture_copy(&self, encoder: &mut wgpu::CommandEncoder) -> CaptureMeta {
        let gpu = self.expect_gpu();
        let mut targets = gpu
            .targets
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        prepare_capture_copy(&gpu.device, &mut targets, encoder)
    }

    /// Map the readback buffer prepared by [`Self::record_capture_copy`]
    /// and return the encoded PNG. Call after the encoder containing
    /// the matching `record_capture_copy` has been submitted.
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called or if the targets
    /// mutex is poisoned — fail-fast per ADR-0063.
    pub fn finish_capture(&self, meta: &CaptureMeta) -> Result<Vec<u8>, String> {
        let gpu = self.expect_gpu();
        let targets = gpu
            .targets
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        finish_capture(&gpu.device, &targets, meta)
    }

    /// Map the readback buffer prepared by [`Self::record_capture_copy`]
    /// and return the raw de-padded RGBA8 frame — the exact pixels
    /// [`Self::finish_capture`] PNG-encodes. The bundle render thread
    /// scores a verdict on these bytes and encodes the PNG from the
    /// same buffer, so the readback is mapped just once
    /// (iamacoffeepot/aether#1777). Call after the encoder containing
    /// the matching `record_capture_copy` has been submitted.
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called or if the targets
    /// mutex is poisoned — fail-fast per ADR-0063.
    pub fn map_capture_rgba(&self, meta: &CaptureMeta) -> Result<Vec<u8>, String> {
        let gpu = self.expect_gpu();
        let targets = gpu
            .targets
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        map_capture_rgba(&gpu.device, &targets, meta)
    }

    /// Resize the offscreen color + depth targets. Idempotent on
    /// zero dimensions (matches winit's `Resized(0, 0)` on minimize).
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called or if the targets
    /// mutex is poisoned — fail-fast per ADR-0063.
    pub fn resize(&self, width: u32, height: u32) {
        let gpu = self.expect_gpu();
        let mut targets = gpu
            .targets
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        targets.resize(&gpu.device, width, height);
    }

    /// Cloned `Arc<wgpu::Device>`. Drivers that need the device for
    /// their own pipelines (e.g. desktop's wireframe overlay pipeline,
    /// swapchain blit) clone here.
    #[must_use]
    pub fn device(&self) -> Arc<wgpu::Device> {
        Arc::clone(&self.expect_gpu().device)
    }

    /// Cloned `Arc<wgpu::Queue>`. Drivers submit through this; the
    /// shared queue means render's `record_frame` writes and the
    /// driver's swapchain submit go through the same submission
    /// order.
    #[must_use]
    pub fn queue(&self) -> Arc<wgpu::Queue> {
        Arc::clone(&self.expect_gpu().queue)
    }

    /// Format the offscreen color target was created with. Capture's
    /// BGRA-vs-RGBA decision keys on this; desktop's swapchain blit
    /// matches its surface format against this to pick a direct copy
    /// vs a manual swizzle.
    #[must_use]
    pub fn color_format(&self) -> wgpu::TextureFormat {
        self.expect_gpu().color_format
    }

    /// Current offscreen color target dimensions. Drivers reading
    /// after a `resize` see the new dimensions immediately.
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called or if the targets
    /// mutex is poisoned — fail-fast per ADR-0063.
    #[must_use]
    pub fn color_size(&self) -> (u32, u32) {
        let targets = self
            .expect_gpu()
            .targets
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        (targets.width(), targets.height())
    }

    /// Run `f` with a borrow of the offscreen color texture. Used by
    /// desktop's swapchain blit: the closure body holds the targets
    /// mutex, so any encoder commands recorded inside are sequenced
    /// against any concurrent resize. Test-bench reaches the
    /// offscreen via the capture path and doesn't need this.
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called or if the targets
    /// mutex is poisoned — fail-fast per ADR-0063.
    pub fn with_color_texture<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&wgpu::Texture) -> R,
    {
        let gpu = self.expect_gpu();
        let targets = gpu
            .targets
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        f(targets.color_texture())
    }
}

/// Bundle of wgpu resources `RenderHandles` exposes post-install.
/// Constructed by the driver from a wgpu device + queue obtained via
/// `Adapter::request_device` (desktop: with surface compatibility;
/// test-bench: offscreen-only). Holds the pipeline + offscreen
/// targets so encoder-level methods can record draws and capture
/// copies without the driver threading these through every call.
pub struct RenderGpu {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub pipeline: Pipeline,
    /// Textured-quad overlay pipeline (ADR-0105). Built alongside the
    /// main pipeline so `record_overlay_pass` can draw the
    /// accumulated quads into the same offscreen target after the
    /// world pass.
    pub quad_pipeline: QuadPipeline,
    pub targets: Mutex<Targets>,
    pub color_format: wgpu::TextureFormat,
}

impl RenderGpu {
    /// Build the standard render pipeline + offscreen targets at the
    /// given size and pass [`Self`] to [`RenderHandles::install_gpu`].
    /// `polygon_mode` is `Fill` for the normal case; desktop's
    /// `AETHER_WIREFRAME=line` chassis env passes `Line` so the main
    /// pipeline draws as wireframe instead of building a separate
    /// overlay pipeline.
    #[must_use]
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        color_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        polygon_mode: wgpu::PolygonMode,
    ) -> Self {
        let pipeline = build_main_pipeline(&device, &queue, color_format, polygon_mode);
        let quad_pipeline = build_quad_pipeline(&device, color_format);
        let targets = Targets::new(&device, color_format, width, height);
        Self {
            device,
            queue,
            pipeline,
            quad_pipeline,
            targets: Mutex::new(targets),
            color_format,
        }
    }
}
