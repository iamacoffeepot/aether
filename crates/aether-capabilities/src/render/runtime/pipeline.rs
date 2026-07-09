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
    CaptureMeta, MATERIAL_VERTEX_STRIDE, MATERIAL_VERTICES_PER_RECT, MaterialDraw,
    MaterialPassDraw, MaterialPassRecord, MaterialPipelines, OverlayDraw, Pipeline,
    QUAD_VERTEX_STRIDE, QUAD_VERTICES_PER_QUAD, QuadPipeline, RenderError, Targets,
    TextureBindings, build_main_pipeline, build_material_pipelines, build_quad_pipeline,
    build_texture_bindings, finish_capture, map_capture_rgba, prepare_capture_copy,
    push_coverage_params, push_material_rect_vertices, push_screen_quad_vertices,
    push_textured_params, push_world_quad_vertices, record_main_pass, record_material_pass,
    record_quad_overlay_pass,
};

use super::material::{MaterialBatch, accepts_coverage_texture};
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
    /// textured_quads` pushes a `QuadBatch` here; `record_overlay_
    /// pass` consumes by swapping with `quad_last_submitted` — the
    /// same immediate-mode cache the triangle path uses, so a
    /// `TestBench::capture` replays the last committed quads.
    pub quad_frame: Arc<Mutex<Vec<QuadBatch>>>,
    /// Most-recently-rendered quad batches, kept across frames so an
    /// idle `capture` (no producer this frame) replays them, matching
    /// `last_submitted`'s role for triangles.
    pub quad_last_submitted: Arc<Mutex<Vec<QuadBatch>>>,
    /// Per-frame material accumulator (ADR-0140), holding both typed
    /// material kinds in receive order so mixed material submissions
    /// preserve painter's order among overlapping surfaces.
    pub material_frame: Arc<Mutex<Vec<MaterialBatch>>>,
    /// Most-recently-rendered material batches for idle capture replay.
    pub material_last_submitted: Arc<Mutex<Vec<MaterialBatch>>>,
    /// Session-scoped texture registry: staged CPU pixels + lazily-
    /// realized GPU textures. Written by the cap dispatcher thread
    /// (`create_texture` / `update_texture`), realized + read by the
    /// driver thread at record time.
    pub textures: Arc<Mutex<TextureRegistry>>,
    /// wgpu state, installed post-cap-construction by the driver via
    /// [`Self::install_gpu`]. Boots empty because winit 0.30's
    /// `ActiveEventLoop::create_window` only fires inside `resumed`,
    /// after `Builder::build` has returned. Test-bench (no surface)
    /// installs immediately after `build_passive`; desktop installs
    /// in its `resumed` handler. Encoder-level methods panic if
    /// called before install — in practice every code path that
    /// calls them runs after the install site.
    pub gpu: Arc<OnceLock<RenderGpu>>,
    /// Resolved per-frame vertex buffer cap (`RenderConfig::
    /// vertex_buffer_bytes`), carried here so the driver sizes the GPU
    /// vertex buffer ([`RenderGpu::new`]) to the same value the cap's
    /// accumulator truncates at.
    pub vertex_buffer_bytes: usize,
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
                entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
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
                            quad.tint.to_array(),
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
                            quad.tint.to_array(),
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
                clip: batch
                    .clip
                    .as_ref()
                    .map(|clip| [clip.x, clip.y, clip.width, clip.height]),
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

    /// Record the depth-tested world-space material pass (ADR-0140)
    /// between the main world pass and the screen overlay. Textures are
    /// realized lazily from the shared registry. Coverage draws require
    /// R8 textures and warn-drop otherwise.
    ///
    /// # Panics
    /// Panics if `install_gpu` hasn't been called, or if any internal
    /// mutex is poisoned — fail-fast per ADR-0063.
    #[allow(clippy::too_many_lines)]
    pub fn record_material_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        replay_cache_when_idle: bool,
    ) {
        let gpu = self.expect_gpu();
        commit_or_replay(
            &self.material_frame,
            &self.material_last_submitted,
            replay_cache_when_idle,
        );
        let batches = self
            .material_last_submitted
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
        let mut registry = self
            .textures
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");

        for batch in &batches {
            let texture_id = match batch {
                MaterialBatch::Textured { texture_id, .. }
                | MaterialBatch::Coverage { texture_id, .. } => *texture_id,
            };
            if let Some(entry) = registry.entries.get_mut(&texture_id) {
                entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
            } else {
                tracing::warn!(
                    target: "aether_capabilities::render",
                    texture_id,
                    "material draw for unknown texture id; dropping the batch",
                );
            }
        }

        let mut vertex_bytes = Vec::new();
        let mut textured_params = Vec::new();
        let mut coverage_params = Vec::new();
        let mut draws = Vec::new();
        let vertex_count =
            u32::try_from(MATERIAL_VERTICES_PER_RECT).expect("material rect vertex count fits u32");
        for batch in &batches {
            match batch {
                MaterialBatch::Textured { texture_id, rects } => {
                    let Some(entry) = registry.entries.get(texture_id) else {
                        continue;
                    };
                    let Some(realized) = entry.realized.as_ref() else {
                        continue;
                    };
                    for rect in rects {
                        let Some(params_offset) =
                            push_textured_params(&mut textured_params, rect.tint.to_array())
                        else {
                            tracing::warn!(
                                target: "aether_capabilities::render",
                                texture_id,
                                "textured material params overflow; dropping rect",
                            );
                            continue;
                        };
                        #[allow(clippy::cast_possible_truncation)]
                        let first_vertex =
                            (vertex_bytes.len() / MATERIAL_VERTEX_STRIDE as usize) as u32;
                        push_material_rect_vertices(
                            &mut vertex_bytes,
                            [
                                rect.rect.x,
                                rect.rect.y,
                                rect.rect.width,
                                rect.rect.height,
                                rect.rect.z,
                            ],
                            [rect.u0, rect.v0, rect.u1, rect.v1],
                        );
                        draws.push(MaterialPassDraw::Textured(MaterialDraw {
                            bind_group: realized.bind_group(),
                            first_vertex,
                            vertex_count,
                            params_offset,
                        }));
                    }
                }
                MaterialBatch::Coverage { texture_id, rects } => {
                    let Some(entry) = registry.entries.get(texture_id) else {
                        continue;
                    };
                    if !accepts_coverage_texture(entry.format) {
                        tracing::warn!(
                            target: "aether_capabilities::render",
                            texture_id,
                            ?entry.format,
                            "coverage material requires an R8 texture; dropping the batch",
                        );
                        continue;
                    }
                    let Some(realized) = entry.realized.as_ref() else {
                        continue;
                    };
                    for rect in rects {
                        let Some(params_offset) = push_coverage_params(
                            &mut coverage_params,
                            rect.body_color.to_array(),
                            rect.rim_color.to_array(),
                            rect.rim_width,
                        ) else {
                            tracing::warn!(
                                target: "aether_capabilities::render",
                                texture_id,
                                "coverage material params overflow; dropping rect",
                            );
                            continue;
                        };
                        #[allow(clippy::cast_possible_truncation)]
                        let first_vertex =
                            (vertex_bytes.len() / MATERIAL_VERTEX_STRIDE as usize) as u32;
                        push_material_rect_vertices(
                            &mut vertex_bytes,
                            [
                                rect.rect.x,
                                rect.rect.y,
                                rect.rect.width,
                                rect.rect.height,
                                rect.rect.z,
                            ],
                            [0.0, 0.0, 1.0, 1.0],
                        );
                        draws.push(MaterialPassDraw::Coverage(MaterialDraw {
                            bind_group: realized.bind_group(),
                            first_vertex,
                            vertex_count,
                            params_offset,
                        }));
                    }
                }
            }
        }

        record_material_pass(
            encoder,
            MaterialPassRecord {
                queue: &gpu.queue,
                pipeline: &gpu.material_pipelines,
                main_pipeline: &gpu.pipeline,
                targets: &targets,
                vertex_bytes: &vertex_bytes,
                draws: &draws,
                textured_params: &textured_params,
                coverage_params: &coverage_params,
            },
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
    /// Shared texture+sampler bindings used by every texture-sampling
    /// pipeline. The quad overlay owns the first consumer; material
    /// pipelines added by ADR-0140 use the same layout object.
    pub texture_bindings: TextureBindings,
    /// Textured-quad overlay pipeline (ADR-0105). Built alongside the
    /// main pipeline so `record_overlay_pass` can draw the
    /// accumulated quads into the same offscreen target after the
    /// world pass.
    pub quad_pipeline: QuadPipeline,
    /// Depth-tested material pipelines (ADR-0140), recorded after the
    /// main pass and before the quad overlay.
    pub material_pipelines: MaterialPipelines,
    pub targets: Mutex<Targets>,
    pub color_format: wgpu::TextureFormat,
}

impl RenderGpu {
    /// Build the standard render pipeline + offscreen targets at the
    /// given size and pass [`Self`] to [`RenderHandles::install_gpu`].
    /// `polygon_mode` is `Fill` for the normal case; desktop's
    /// `AETHER_WIREFRAME=line` chassis env passes `Line` so the main
    /// pipeline draws as wireframe instead of building a separate
    /// overlay pipeline. `vertex_buffer_bytes` sizes the per-frame GPU
    /// vertex buffer — drivers pass
    /// [`RenderHandles::vertex_buffer_bytes`] so the buffer matches the
    /// cap accumulator's resolved truncation cap.
    #[must_use]
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        color_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        polygon_mode: wgpu::PolygonMode,
        vertex_buffer_bytes: usize,
    ) -> Self {
        let pipeline = build_main_pipeline(
            &device,
            &queue,
            color_format,
            polygon_mode,
            vertex_buffer_bytes,
        );
        let texture_bindings = build_texture_bindings(&device);
        let quad_pipeline = build_quad_pipeline(&device, color_format, &texture_bindings);
        let material_pipelines = build_material_pipelines(
            &device,
            color_format,
            &pipeline.camera_bind_group_layout,
            &texture_bindings,
        );
        let targets = Targets::new(&device, color_format, width, height);
        Self {
            device,
            queue,
            pipeline,
            texture_bindings,
            quad_pipeline,
            material_pipelines,
            targets: Mutex::new(targets),
            color_format,
        }
    }
}
