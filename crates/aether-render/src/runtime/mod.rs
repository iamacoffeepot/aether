//! The pumped `aether.render` runtime half (ADR-0122 identity/runtime
//! split, ADR-0161). Compiled only under `feature = "render-runtime"` (the
//! `mod runtime;` declaration in the parent carries the gate), so a
//! marker-only `render` build of the [`RenderCapability`] identity never
//! names these types nor pulls the wgpu-bound substrate runtime through this
//! cap.
//!
//! [`RenderCapability`] is a *pumped* actor (ADR-0160), dispatched on the
//! chassis driver thread, so it owns every accumulator as a plain field and
//! the GPU + pending capture outright: frame recording, capture readback,
//! and present all run on the one thread that owns the surfaces. The three
//! chassis-internal kinds ([`Frame`], [`PreSettled`], [`Occluded`], defined
//! in [`crate::kinds`]) turn frame invocation, pre-mail settlement, and
//! window occlusion into mail — so every capture transition is a handler
//! with trace brackets and a cost row, and the capture state machine is
//! testable headlessly with a toy pump.
//!
//! Gated on `runtime`: the substrate harness builds the **offscreen**
//! (surfaceless) path without winit — the windowed boot inside is
//! `desktop`-gated line by line.
//!
//! ## Capture bridge notes (ADR-0161):
//! - **Settlement bridge.** [`on_capture_frame`](RenderCapability::on_capture_frame)
//!   bridges each pre-mail settlement to a [`PreSettled`] mail through
//!   [`SettlementRegistry::subscribe_settlement_mail`], which pushes a
//!   settlement-notice mail from whatever thread the settlement fires on. A
//!   render handler must never block on a pre-mail settlement (the ADR
//!   deadlock: pre-chains terminate back at this mailbox), so the bridge only
//!   mails. `PreSettled` is wire-identical to `aether.trace.settled` (a single
//!   `MailId` field), so the pushed notice decodes as `PreSettled`.
//! - **Capture scoring.** `FrameCheck` verdicts and similarity scoring live in
//!   [`aether_substrate::render::visual`], so the ready-branch readback scores
//!   the verdict and similarity directly.

use std::collections::BTreeSet;
use std::io;
use std::iter;
use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_actor::runtime;
pub use aether_data::Kind;

use aether_kinds::{CaptureFrame, CaptureFrameResult, WindowId};

use aether_substrate::Manual;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::helpers::resolve_bundle;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use aether_substrate::render::visual;
use aether_substrate::render::{
    CaptureMeta, IDENTITY_VIEW_PROJ, RenderError, encode_png, map_capture_rgba, prepare_capture_copy, record_main_pass,
};
#[cfg(feature = "desktop")]
use winit::window::Window;

// The native impl seams, nested under this `runtime` directory so the one
// `mod runtime;` gate in the parent covers them (no per-sibling `#[cfg]`):
// `pipeline` (GPU bundle + shared record helpers), `texture` (the texture
// registry), `quad` (the quad-batch accumulator), `material` (the
// material-batch accumulator), `capture` (the similarity-reference resolver),
// and `config` (the `RenderTuningConfig` knobs + `RenderParams`).
mod capture;
mod config;
// The `HeadlessRenderCapability` companion's runtime half (identity in the
// crate-root `headless` module) — a nested child so the same `mod runtime;`
// gate covers it.
mod headless;
mod material;
mod pipeline;
mod quad;
// Shared desktop-surface GPU helpers (ADR-0161): the wireframe overlay
// pipeline builder, swapchain acquisition, and the surface / offscreen
// device boot, called by the pumped render runtime.
mod surface;
// Per-window render targets and the id-keyed map over them; desktop-only, so
// unlike its siblings this one carries the feature gate.
#[cfg(feature = "desktop")]
mod target;
mod texture;

// The cap-root re-exports source these names through `runtime`. The
// `RenderTuning*` trio is the derive-Config surface (ADR-0090) the chassis
// resolves the render boot knobs through; `RenderParams` is the composer
// wiring channel.
pub use self::config::{RenderParams, RenderTuningConfig, RenderTuningConfigLayer, RenderTuningOverlay};
pub use self::pipeline::RenderGpu;

use self::pipeline::{record_material_batches, record_overlay_batches};
use self::surface::{boot_offscreen, build_wireframe_overlay_pipeline};
#[cfg(feature = "desktop")]
use self::target::{DesktopGpuContext, FirstWindowGpu, RenderTarget, WindowTargets};

// These seam items are `pub` (visible in `render`) in their now-nested child
// modules, so the re-export up to runtime level keeps that exact visibility.
use self::capture::PendingCapture;
pub use self::capture::resolve_reference;
pub use self::material::MaterialBatch;
pub use self::quad::QuadBatch;
pub use self::texture::{TextureRegistry, WHITE_TEXTURE_ID};

use super::{
    CreateTexture, CreateTextureResult, DRAW_TRIANGLE_BYTES, DestroyTexture, DrawMaterialCoverage,
    DrawMaterialTextured, DrawSolidQuads, DrawTexturedQuads, DrawTriangle, Frame, Occluded, PreSettled,
    RenderCapability, UpdateTexture, ViewProjection,
};

/// Wedge-to-`Err` cap for a parked capture (ADR-0161): if a capture's
/// pre-mail chain has not settled within this window the next frame past
/// the deadline replies `Err`, reproducing the `FRAME_SETTLEMENT_CAP`
/// disposition event-driven. Matches the desktop driver's 30s
/// advance-settlement bound.
const FRAME_SETTLEMENT_CAP: Duration = Duration::from_secs(30);

/// Pumped `aether.render` runtime state (ADR-0161). Owns the accumulators,
/// the shared GPU + window-keyed surfaces, and the pending capture as plain
/// fields. The
/// addressing identity is the distinct ZST [`super::RenderCapability`].
pub struct RenderCapabilityState {
    frame_vertices: Vec<u8>,
    last_submitted: Vec<u8>,
    triangles_rendered: u64,
    camera_state: [f32; 16],
    quad_frame: Vec<QuadBatch>,
    quad_last_submitted: Vec<QuadBatch>,
    material_frame: Vec<MaterialBatch>,
    material_last_submitted: Vec<MaterialBatch>,
    textures: TextureRegistry,
    vertex_buffer_bytes: usize,

    /// Attached desktop surfaces keyed by the canonical engine window id.
    #[cfg(feature = "desktop")]
    targets: WindowTargets<RenderTarget>,
    /// Instance/adapter selected by the first successful window attachment;
    /// retained so later surfaces negotiate against the same device.
    #[cfg(feature = "desktop")]
    desktop_gpu: Option<DesktopGpuContext>,
    /// ADR-0161 R4: offscreen boot dimensions. `Some((w, h))` makes the
    /// first target-free `on_frame` boot a surfaceless GPU at these
    /// dimensions — the substrate harness's path.
    offscreen_size: Option<(u32, u32)>,
    /// Resolved `AETHER_WIREFRAME` value threaded from params.
    wireframe: Option<String>,
    /// Shared wgpu device, pipelines, and reusable offscreen target. Desktop
    /// boots it transactionally with the first window attachment; the
    /// harness boots it lazily from `offscreen_size`.
    gpu: Option<RenderGpu>,
    wire_pipeline: Option<wgpu::RenderPipeline>,
    /// Prior frame's submission index, drained at the top of the next
    /// frame to bound the present loop to one frame in flight (issue 1312).
    last_submission: Option<wgpu::SubmissionIndex>,
    /// ADR-0161 R4: committed-overlay observation sink for the substrate
    /// harness's `committed_overlay_snapshot`. `record_overlay_batches`
    /// populates it with the batches that *survived* record-time rejection
    /// (missing texture / invalid clip / past budget), so the snapshot
    /// reflects what was drawn — not the raw accumulator. `Mutex` only
    /// because `record_overlay_batches` takes `&Mutex<_>` (the harness sink's
    /// shape); the pumped state is single-threaded, so it never contends.
    overlay_observation: Mutex<Vec<DrawTexturedQuads>>,

    pending_capture: Option<PendingCapture>,

    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    /// Reply edge for `on_capture_frame`'s inline-failure paths (occluded,
    /// already-pending, bundle / reference resolution error) — those reply
    /// synchronously and let the dispatcher settle the inbound, so they
    /// never take the deferred guard.
    outbound: Arc<HubOutbound>,
    assets_dir: Option<PathBuf>,
    observed_kinds: Option<Arc<Mutex<Vec<String>>>>,
}

impl RenderCapabilityState {
    /// Deadline of the pending capture, if one is parked (ADR-0161
    /// §Decision 4). Read by the driver through
    /// [`PumpedSlot::read_state`](aether_substrate::actor::native::PumpedSlot::read_state)
    /// so it can park with `ControlFlow::WaitUntil(deadline)` — the single
    /// capture-awareness the driver retains, so a wedged pre-chain on a
    /// parked window still reaches the deadline check.
    #[must_use]
    pub fn capture_deadline(&self) -> Option<Instant> {
        self.pending_capture.as_ref().map(|pending| pending.deadline)
    }

    /// Cumulative triangle count this session, for the driver's shutdown FPS
    /// report (ADR-0161). The pumped runtime owns `triangles_rendered` as
    /// plain state, so the driver reads it through
    /// [`PumpedSlot::read_state`](aether_substrate::actor::native::PumpedSlot::read_state)
    /// before `shutdown` consumes the actor; `unwire` logs the same count.
    #[must_use]
    pub fn triangles_rendered(&self) -> u64 {
        self.triangles_rendered
    }

    /// Whether a parked capture is **ready to read back** — every pre-mail
    /// chain has settled (`pre_remaining == 0`), so the draws those chains
    /// terminate at have already dispatched onto the owned accumulators
    /// (ADR-0161 R4). Read by the substrate harness's frame hook through
    /// `PumpedSlot::read_state` to decide when to drive the capture frame:
    /// the harness sends `aether.render.frame` only once this is `true`, so
    /// the record never runs against an accumulator a still-in-flight pre-mail
    /// chain has yet to fill. The pumped state's fields are private, so this
    /// is the read surface the pump-owning thread uses.
    #[must_use]
    pub fn capture_ready(&self) -> bool {
        self.pending_capture.as_ref().is_some_and(PendingCapture::is_ready)
    }

    /// Snapshot the ordered overlay batches from the most recently committed
    /// frame as their public [`DrawTexturedQuads`] shape (ADR-0105 /
    /// ADR-0161 R4). Reads the observation sink `record_overlay_batches`
    /// populates, so batches rejected at record time (missing texture,
    /// invalid/empty clip, past the vertex budget) are excluded and solid
    /// submissions appear normalized over the reserved white texture. Read by
    /// the harness's `committed_overlay_snapshot` extension through
    /// `PumpedSlot::read_state`. Owns its data.
    ///
    /// # Panics
    /// Panics if the observation mutex is poisoned — fail-fast per ADR-0063.
    #[must_use]
    pub fn committed_overlay_snapshot(&self) -> Vec<DrawTexturedQuads> {
        self.overlay_observation.lock().expect("mutex poisoned; fail-fast per ADR-0063").clone()
    }

    /// Push a dispatched kind name into the `SubstrateHarness` observation
    /// sink, when one is installed. Production chassis leave it `None`.
    fn observe(&self, name: &str) {
        if let Some(obs) = &self.observed_kinds {
            obs.lock().expect("mutex poisoned; fail-fast per ADR-0063").push(name.into());
        }
    }

    /// Attach one native window as a render target. The first attachment
    /// selects the adapter/device and builds shared pipelines; later
    /// attachments must support the same copy-compatible color format.
    /// Every fallible operation completes before insertion, so failure leaves
    /// both the target map and shared GPU state unchanged.
    #[cfg(feature = "desktop")]
    pub fn attach_window(&mut self, id: WindowId, window: Arc<Window>) -> Result<(), String> {
        if self.offscreen_size.is_some() {
            return Err("cannot attach a window target to an explicitly surfaceless render runtime".to_owned());
        }
        let size = window.inner_size();
        let wireframe = self.wireframe.clone();
        let vertex_buffer_bytes = self.vertex_buffer_bytes;

        let install = if let (Some(gpu), Some(context)) = (self.gpu.as_ref(), self.desktop_gpu.as_ref()) {
            let device = Arc::clone(&gpu.device);
            let format = gpu.color_format;
            self.targets.attach_with(id, || {
                RenderTarget::attach_to_booted_gpu(context, &device, window, (size.width, size.height), format)
            })?
        } else if self.gpu.is_none() && self.desktop_gpu.is_none() {
            self.targets.attach_with(id, || {
                RenderTarget::boot_first(window, (size.width, size.height), wireframe.as_deref(), vertex_buffer_bytes)
            })?
        } else {
            return Err("render GPU boot state cannot accept desktop window targets".to_owned());
        };

        if let Some(FirstWindowGpu { context, gpu, wire_pipeline }) = install {
            self.desktop_gpu = Some(context);
            self.gpu = Some(gpu);
            self.wire_pipeline = wire_pipeline;
        }
        Ok(())
    }

    /// Detach one window surface. A capture selected for that target fails
    /// immediately; captures for other targets and the shared scene survive.
    #[cfg(feature = "desktop")]
    pub fn detach_window(&mut self, id: WindowId) -> bool {
        let removed = self.targets.detach(id).is_some();
        if removed {
            self.fail_capture_for_detached_window(id);
        }
        removed
    }

    #[cfg(feature = "desktop")]
    fn fail_capture_for_detached_window(&mut self, id: WindowId) {
        if self.pending_capture.as_ref().is_some_and(|pending| pending.window == Some(id)) {
            let pending = self.pending_capture.take().expect("just checked Some");
            pending.reply.reply(&CaptureFrameResult::Err {
                error: format!("capture_frame failed: window target {} detached before capture", id.0),
            });
        }
    }

    fn validate_capture_target(&self, window: Option<WindowId>) -> Result<(), String> {
        #[cfg(feature = "desktop")]
        {
            if self.targets.validate_capture_selection(window, |target| target.occluded)? {
                return Ok(());
            }
        }
        #[cfg(not(feature = "desktop"))]
        if let Some(id) = window {
            return Err(format!("capture_frame failed: window target {} is unavailable on this render runtime", id.0));
        }
        if self.offscreen_size.is_some() {
            Ok(())
        } else {
            Err("capture_frame failed: no surfaceless capture target is configured".to_owned())
        }
    }

    /// Boot the explicit surfaceless harness GPU. Desktop GPUs are booted by
    /// `attach_window`, never by a frame or a shared handle.
    fn ensure_offscreen_gpu_booted(&mut self) {
        if self.gpu.is_some() {
            return;
        }
        let Some((width, height)) = self.offscreen_size else {
            return;
        };
        let booted = boot_offscreen(self.wireframe.as_deref());
        let gpu = RenderGpu::new(
            Arc::clone(&booted.device),
            Arc::clone(&booted.queue),
            booted.format,
            width,
            height,
            booted.polygon_mode,
            self.vertex_buffer_bytes,
        );
        self.wire_pipeline = booted
            .build_overlay
            .then(|| build_wireframe_overlay_pipeline(&booted.device, gpu.color_format, &gpu.pipeline.pipeline_layout));
        self.gpu = Some(gpu);
    }

    fn commit_scene(&mut self, replay_cache_when_idle: bool) {
        commit_or_replay(&mut self.frame_vertices, &mut self.last_submitted, replay_cache_when_idle);
        commit_or_replay(&mut self.material_frame, &mut self.material_last_submitted, replay_cache_when_idle);
        commit_or_replay(&mut self.quad_frame, &mut self.quad_last_submitted, replay_cache_when_idle);
    }

    /// Record the world / material / overlay passes into `encoder` from the
    /// already-committed global scene. The caller may invoke it once per
    /// dirty target at that target's dimensions without consuming the scene
    /// again.
    fn record_passes(&mut self, encoder: &mut wgpu::CommandEncoder) -> Result<(), RenderError> {
        let gpu = self.gpu.as_ref().expect("record_passes requires a booted GPU");
        let extras_storage: [&wgpu::RenderPipeline; 1];
        let extras: &[&wgpu::RenderPipeline] = match self.wire_pipeline.as_ref() {
            Some(pipeline) => {
                extras_storage = [pipeline];
                &extras_storage
            }
            None => &[],
        };
        // World pass — writes the camera uniform the material pass reads.
        {
            let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            record_main_pass(
                &gpu.queue,
                encoder,
                &gpu.pipeline,
                &targets,
                &self.last_submitted,
                &self.camera_state,
                extras,
            )?;
        }
        // Material pass (depth-tested world-space rects), then the screen /
        // world overlay pass.
        {
            let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            record_material_batches(gpu, encoder, &targets, &mut self.textures, &self.material_last_submitted);
        }
        {
            let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            record_overlay_batches(
                gpu,
                encoder,
                &targets,
                &mut self.textures,
                &self.quad_last_submitted,
                self.camera_state,
                Some(&self.overlay_observation),
            );
        }
        Ok(())
    }

    fn record_target_frame(
        &mut self,
        width: u32,
        height: u32,
        surface_texture: Option<wgpu::SurfaceTexture>,
        capture: bool,
    ) -> Result<Option<CaptureMeta>, RenderError> {
        let gpu = self.gpu.as_ref().expect("record_target_frame requires a booted GPU");
        let device = Arc::clone(&gpu.device);
        let queue = Arc::clone(&gpu.queue);
        {
            let mut targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            if targets.width() != width || targets.height() != height {
                targets.resize(&device, width, height);
            }
        }

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame encoder") });
        self.record_passes(&mut encoder)?;
        let capture_meta = capture.then(|| {
            let gpu = self.gpu.as_ref().expect("gpu present in this branch");
            let mut targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            prepare_capture_copy(&gpu.device, &mut targets, &mut encoder)
        });

        if let Some(texture) = surface_texture.as_ref() {
            let gpu = self.gpu.as_ref().expect("gpu present in this branch");
            let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: targets.color_texture(),
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &texture.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }

        self.last_submission = Some(queue.submit(iter::once(encoder.finish())));
        if let Some(texture) = surface_texture {
            texture.present();
        }
        Ok(capture_meta)
    }

    fn complete_capture(&mut self, meta: CaptureMeta) {
        let pending = self.pending_capture.take().expect("capture metadata requires a pending capture");
        for mail in pending.after_mails {
            self.mailer.push(mail);
        }
        let gpu = self.gpu.as_ref().expect("capture metadata requires a booted GPU");
        let outcome: Result<CaptureFrameResult, String> = {
            let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            map_capture_rgba(&gpu.device, &targets, &meta).and_then(|rgba| {
                let png = encode_png(&rgba, meta.width, meta.height)?;
                let (similarity_score, similarity_pass) =
                    visual::score_similarity(&rgba, meta.width, meta.height, pending.reference.as_ref())?;
                let verdict = (!pending.checks.is_empty())
                    .then(|| visual::run_checks(rgba, meta.width, meta.height, &pending.checks));
                Ok(CaptureFrameResult::Ok { png, verdict, similarity_score, similarity_pass })
            })
        };
        match outcome {
            Ok(result) => pending.reply.reply(&result),
            Err(error) => pending.reply.reply(&CaptureFrameResult::Err { error }),
        };
    }
}

/// Owned-field commit-or-replay (ADR-0161 §Scope: "a bare `mem::swap`").
/// - `live` non-empty → swap it into `last` and clear `live` for next frame.
/// - `live` empty, `!replay_cache_when_idle` → clear `last` (commit-current).
/// - `live` empty, `replay_cache_when_idle` → leave `last` (replay-cache).
fn commit_or_replay<T>(live: &mut Vec<T>, last: &mut Vec<T>, replay_cache_when_idle: bool) {
    if !live.is_empty() {
        mem::swap(live, last);
        live.clear();
    } else if !replay_cache_when_idle {
        last.clear();
    }
}

fn deduplicate_windows(windows: Vec<WindowId>) -> BTreeSet<WindowId> {
    windows.into_iter().collect()
}

#[runtime]
impl NativeActor for RenderCapability {
    type State = RenderCapabilityState;
    type Config = RenderTuningConfig;
    type Params = RenderParams;

    const NAMESPACE: &'static str = "aether.render";

    fn init(
        config: RenderTuningConfig,
        params: RenderParams,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<RenderCapabilityState, BootError> {
        let mailer = ctx.mailer();
        let registry = Arc::clone(mailer.registry());
        let outbound = mailer.outbound().cloned().ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "HubOutbound must be wired on Mailer before RenderCapability::init",
            )))
        })?;
        Ok(RenderCapabilityState {
            frame_vertices: Vec::with_capacity(config.vertex_buffer_bytes),
            last_submitted: Vec::with_capacity(config.vertex_buffer_bytes),
            triangles_rendered: 0,
            camera_state: IDENTITY_VIEW_PROJ,
            quad_frame: Vec::new(),
            quad_last_submitted: Vec::new(),
            material_frame: Vec::new(),
            material_last_submitted: Vec::new(),
            textures: TextureRegistry::new(),
            vertex_buffer_bytes: config.vertex_buffer_bytes,
            #[cfg(feature = "desktop")]
            targets: WindowTargets::default(),
            #[cfg(feature = "desktop")]
            desktop_gpu: None,
            offscreen_size: params.offscreen_size,
            wireframe: params.wireframe,
            gpu: None,
            wire_pipeline: None,
            last_submission: None,
            overlay_observation: Mutex::new(Vec::new()),
            pending_capture: None,
            registry,
            mailer,
            outbound,
            assets_dir: params.assets_dir,
            observed_kinds: params.observed_kinds,
        })
    }

    /// `DrawTriangle` accumulator, on the owned `frame_vertices` buffer.
    /// Truncates at the cap boundary, rounding to whole triangles.
    #[handler::single]
    fn on_draw_triangle(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mails: &[DrawTriangle]) {
        state.observe(<DrawTriangle as Kind>::NAME);
        let bytes: &[u8] = bytemuck::cast_slice(mails);
        let cap_bytes = state.vertex_buffer_bytes;
        let available = cap_bytes.saturating_sub(state.frame_vertices.len());
        let write_len = bytes.len().min(available);
        let write_len = write_len - (write_len % DRAW_TRIANGLE_BYTES);
        if write_len > 0 {
            state.frame_vertices.extend_from_slice(&bytes[..write_len]);
            state.triangles_rendered += (write_len / DRAW_TRIANGLE_BYTES) as u64;
        }
        if write_len < bytes.len() {
            tracing::warn!(
                target: "aether_substrate::render",
                accepted_bytes = write_len,
                dropped_bytes = bytes.len() - write_len,
                cap = cap_bytes,
                "render cap dropped triangles beyond fixed vertex buffer",
            );
        }
    }

    /// `ViewProjection` latest-value-wins, on the owned `camera_state`.
    #[handler::single]
    fn on_camera(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ViewProjection) {
        state.observe(<ViewProjection as Kind>::NAME);
        state.camera_state = mail.view_proj;
    }

    /// `CreateTexture` (ADR-0105), on the owned texture registry.
    #[handler::single]
    fn on_create_texture(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CreateTexture,
    ) -> CreateTextureResult {
        state.observe(<CreateTexture as Kind>::NAME);
        state.textures.create(mail)
    }

    /// `UpdateTexture` (ADR-0105), on the owned texture registry.
    #[handler::single]
    fn on_update_texture(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UpdateTexture) {
        state.observe(<UpdateTexture as Kind>::NAME);
        state.textures.update(mail);
    }

    /// `DestroyTexture`, on the owned texture registry.
    #[handler::single]
    fn on_destroy_texture(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DestroyTexture) {
        state.observe(<DestroyTexture as Kind>::NAME);
        state.textures.destroy(mail);
    }

    /// `DrawTexturedQuads` accumulator (ADR-0105), on the owned `quad_frame`.
    #[handler::single]
    fn on_draw_textured_quads(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawTexturedQuads) {
        state.observe(<DrawTexturedQuads as Kind>::NAME);
        state.quad_frame.push(QuadBatch::textured(mail));
    }

    /// `DrawSolidQuads` (ADR-0107 §4), on the owned `quad_frame` — expand to
    /// the reserved white texture tinted by `color`.
    #[handler::single]
    fn on_draw_solid_quads(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawSolidQuads) {
        state.observe(<DrawSolidQuads as Kind>::NAME);
        let batch = QuadBatch::solid(mail, &mut state.textures);
        state.quad_frame.push(batch);
    }

    /// `DrawMaterialTextured` (ADR-0140), on the owned material stream.
    #[handler::single]
    fn on_draw_material_textured(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawMaterialTextured) {
        state.observe(<DrawMaterialTextured as Kind>::NAME);
        state.material_frame.push(MaterialBatch::textured(mail));
    }

    /// `DrawMaterialCoverage` (ADR-0140), on the owned material stream.
    #[handler::single]
    fn on_draw_material_coverage(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawMaterialCoverage) {
        state.observe(<DrawMaterialCoverage as Kind>::NAME);
        state.material_frame.push(MaterialBatch::coverage(mail));
    }

    /// `PreSettled` (ADR-0161) — decrement the pending capture's
    /// `pre_remaining`. A stray notice with no pending capture is ignored.
    #[handler::single]
    fn on_pre_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: PreSettled) {
        state.observe(<PreSettled as Kind>::NAME);
        if let Some(pending) = &mut state.pending_capture {
            pending.pre_remaining = pending.pre_remaining.saturating_sub(1);
        }
    }

    /// `Occluded` — update only the named target and fail only a capture
    /// selected for that target.
    #[handler::single]
    fn on_occluded(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Occluded) {
        state.observe(<Occluded as Kind>::NAME);
        #[cfg(feature = "desktop")]
        let became_occluded =
            state.targets.set_occluded(mail.window, mail.occluded, |target, occluded| target.occluded = occluded)
                && mail.occluded;
        #[cfg(not(feature = "desktop"))]
        let became_occluded = false;

        if became_occluded && state.pending_capture.as_ref().is_some_and(|pending| pending.window == Some(mail.window))
        {
            let pending = state.pending_capture.take().expect("just checked Some");
            pending.reply.reply(&CaptureFrameResult::Err {
                error: format!("capture_frame failed: window target {} became occluded before capture", mail.window.0),
            });
        }
    }

    /// `Frame` commits the application-scoped scene once, deduplicates its
    /// dirty window ids, then records and presents that committed scene at
    /// each live non-occluded target's dimensions. A target whose record
    /// fails drops that target's frame and nothing else — the fan-out still
    /// owes every window behind it its turn. An empty target list is
    /// reserved for the explicitly surfaceless harness.
    #[handler::single]
    fn on_frame(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Frame) {
        state.observe(<Frame as Kind>::NAME);
        let Frame { replay_cache_when_idle, windows } = mail;
        let windows = deduplicate_windows(windows);

        // Deadline disposition first — a wedged pre-chain replies `Err`
        // even on a frame where nothing else happens.
        let now = Instant::now();
        if state.pending_capture.as_ref().is_some_and(|pending| pending.is_expired(now)) {
            let pending = state.pending_capture.take().expect("just checked Some");
            pending.reply.reply(&CaptureFrameResult::Err {
                error: "capture_frame failed: pre-mail settlement did not complete within the frame settlement cap"
                    .to_owned(),
            });
        }

        state.commit_scene(replay_cache_when_idle);
        state.ensure_offscreen_gpu_booted();

        let Some(gpu) = state.gpu.as_ref() else {
            if state.pending_capture.as_ref().is_some_and(PendingCapture::is_ready)
                && let Some(pending) = state.pending_capture.take()
            {
                pending.reply.reply(&CaptureFrameResult::Err {
                    error: "capture_frame failed: the render GPU is not booted on this chassis".to_owned(),
                });
            }
            return;
        };
        let device = Arc::clone(&gpu.device);

        // One-frame-in-flight: drain the prior submission before recording
        // any target in the next global frame (issue 1312).
        if let Some(index) = state.last_submission.take()
            && let Err(error) = device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None })
        {
            tracing::warn!(target: "aether_substrate::render", ?error, "device.poll for previous frame failed; continuing");
        }

        #[cfg(feature = "desktop")]
        for window in windows.iter().copied() {
            let prepared = {
                let Some(target) = state.targets.get_mut(window) else {
                    continue;
                };
                target.prepare_frame(&device)
            };
            let Some((width, height, surface_texture)) = prepared else {
                continue;
            };
            let capture = state
                .pending_capture
                .as_ref()
                .is_some_and(|pending| pending.is_ready() && pending.window == Some(window));
            let meta = match state.record_target_frame(width, height, surface_texture, capture) {
                Ok(meta) => meta,
                // A record failure disposes of *this* target's frame only —
                // the fan-out owes every listed window its turn, so the loop
                // moves on instead of abandoning the ones behind it.
                Err(RenderError::VertexBufferOverflow { vertex_bytes, cap }) => {
                    tracing::warn!(
                        target: "aether_substrate::render",
                        window = window.0,
                        vertex_bytes,
                        cap,
                        "dropping this window's frame: vertex bytes exceed the buffer; remaining windows still present",
                    );
                    continue;
                }
            };
            if let Some(meta) = meta {
                state.complete_capture(meta);
            }
        }

        if state.offscreen_size.is_some() && windows.is_empty() {
            let (width, height) = {
                let gpu = state.gpu.as_ref().expect("offscreen booted above");
                let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
                (targets.width(), targets.height())
            };
            let capture =
                state.pending_capture.as_ref().is_some_and(|pending| pending.is_ready() && pending.window.is_none());
            let meta = match state.record_target_frame(width, height, None, capture) {
                Ok(meta) => meta,
                Err(RenderError::VertexBufferOverflow { .. }) => return,
            };
            if let Some(meta) = meta {
                state.complete_capture(meta);
            }
        }
    }

    /// `CaptureFrame` — validate the explicit desktop/offscreen selection,
    /// enforce the one global in-flight limit, then park the mail-driven
    /// capture state machine until the selected target's next dirty frame.
    ///
    /// A render handler must **never** block on a pre-mail settlement (the
    /// ADR Context deadlock: pre-chains terminate back at this mailbox), so
    /// the settlement bridge only mails — it never waits.
    #[handler::manual]
    fn on_capture_frame(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: CaptureFrame) {
        state.observe(<CaptureFrame as Kind>::NAME);
        let sender = ctx.reply_target();

        if let Err(error) = state.validate_capture_target(mail.window) {
            state.outbound.send_reply(sender, &CaptureFrameResult::Err { error });
            return;
        }
        if state.pending_capture.is_some() {
            state.outbound.send_reply(
                sender,
                &CaptureFrameResult::Err {
                    error: "capture already pending; try again once the in-flight request completes".to_owned(),
                },
            );
            return;
        }

        let pre = match resolve_bundle(&state.registry, &mail.mails, "capture bundle") {
            Ok(bundle) => bundle,
            Err(error) => {
                state.outbound.send_reply(sender, &CaptureFrameResult::Err { error });
                return;
            }
        };
        let after = match resolve_bundle(&state.registry, &mail.after_mails, "capture after bundle") {
            Ok(bundle) => bundle,
            Err(error) => {
                state.outbound.send_reply(sender, &CaptureFrameResult::Err { error });
                return;
            }
        };
        let reference = match resolve_reference(state.assets_dir.as_deref(), mail.similarity.as_ref()) {
            Ok(reference) => reference,
            Err(error) => {
                state.outbound.send_reply(sender, &CaptureFrameResult::Err { error });
                return;
            }
        };

        // Dispatch each pre-mail on a fresh chassis-rooted chain (issue
        // 860) and bridge its settlement to a `PreSettled` mail addressed
        // to this render mailbox — pushed from whatever thread the
        // settlement fires on. With no settlement registry (some fixtures)
        // `pre_remaining` stays the number dispatched but nothing decrements
        // it, so such a fixture never gates a capture on settlement.
        let settlement_registry = state.mailer.settlement_registry().cloned();
        let self_id = ctx.self_id();
        let mut pre_remaining = 0usize;
        for envelope in pre {
            let mail_id = ctx.send_envelope_detached(envelope.recipient, envelope.kind, envelope.payload.bytes());
            pre_remaining += 1;
            if let Some(registry) = settlement_registry.as_deref() {
                registry.subscribe_settlement_mail(
                    mail_id,
                    self_id,
                    <PreSettled as Kind>::ID,
                    Arc::clone(&state.mailer),
                );
            }
        }

        let reply = ctx.take_inbound();
        state.pending_capture = Some(PendingCapture {
            window: mail.window,
            reply,
            after_mails: after,
            checks: mail.checks,
            reference,
            pre_remaining,
            deadline: Instant::now() + FRAME_SETTLEMENT_CAP,
        });
    }

    /// Log the session's cumulative triangle count on teardown — the
    /// pumped runtime owns `triangles_rendered` as plain state, so its
    /// natural reader is this actor's `unwire`.
    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        tracing::info!(
            target: "aether_substrate::render",
            triangles_rendered = state.triangles_rendered,
            "pumped render runtime shutting down",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::{SolidQuad, TextureFormat};
    use super::texture::StagedTexture;
    use super::*;
    use aether_data::{KindId, MailId, MailboxId, Source, SourceAddr};
    use aether_data::{SessionToken, Uuid};
    use aether_kinds::QuadSpace;
    use aether_kinds::trace::Nanos;
    use aether_math::Rgba;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::chassis::inbox::SettlingInbox;
    use aether_substrate::mail::registry::OwnedDispatch;
    use aether_substrate::mail::{EgressEvent, MailRef};
    use aether_substrate::testing::{decode_reply, test_mailer_and_rx};
    use std::sync::mpsc;

    fn test_staged_texture(pixels: Vec<u8>) -> StagedTexture {
        StagedTexture { width: 2, height: 2, format: TextureFormat::Rgba8, pixels, realized: None, dirty: true }
    }

    /// Build a `PendingCapture` whose retained guard replies to a Session
    /// source so the toy pump can observe the deferred reply through the
    /// egress channel. The inbound is queued straight onto a
    /// `SettlingInbox` (no route), then drained to the guard.
    fn parked_capture(
        mailer: &Arc<Mailer>,
        window: Option<WindowId>,
        pre_remaining: usize,
        deadline: Instant,
    ) -> PendingCapture {
        let id = MailboxId(0x0CA8);
        let (tx, rx) = mpsc::channel::<Envelope>();
        tx.send(OwnedDispatch::disarmed(
            KindId(0),
            "test.capture.pending".to_owned(),
            None,
            // A Session sender routes the guard's reply to the egress rx.
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil()))),
            MailRef::from(Vec::new()),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            id,
        ))
        .expect("queue the inbound");
        let inbox = SettlingInbox::new(id, rx, Arc::clone(mailer));
        let reply = inbox.try_next().expect("one queued");
        PendingCapture {
            window,
            reply,
            after_mails: Vec::new(),
            checks: Vec::new(),
            reference: None,
            pre_remaining,
            deadline,
        }
    }

    /// A minimal headless state for the capture state-machine tests — no
    /// window, no GPU (`gpu` stays `None`, so the ready branch fails fast
    /// rather than touching an absent adapter).
    fn headless_state(mailer: &Arc<Mailer>) -> RenderCapabilityState {
        let outbound = mailer.outbound().cloned().expect("test_mailer_and_rx wires a loopback outbound");
        RenderCapabilityState {
            frame_vertices: Vec::new(),
            last_submitted: Vec::new(),
            triangles_rendered: 0,
            camera_state: IDENTITY_VIEW_PROJ,
            quad_frame: Vec::new(),
            quad_last_submitted: Vec::new(),
            material_frame: Vec::new(),
            material_last_submitted: Vec::new(),
            textures: TextureRegistry::new(),
            vertex_buffer_bytes: 1024,
            #[cfg(feature = "desktop")]
            targets: WindowTargets::default(),
            #[cfg(feature = "desktop")]
            desktop_gpu: None,
            offscreen_size: None,
            wireframe: None,
            gpu: None,
            wire_pipeline: None,
            last_submission: None,
            overlay_observation: Mutex::new(Vec::new()),
            pending_capture: None,
            registry: Arc::clone(mailer.registry()),
            mailer: Arc::clone(mailer),
            outbound,
            assets_dir: None,
            observed_kinds: None,
        }
    }

    fn ctx_binding(mailer: &Arc<Mailer>) -> Arc<NativeBinding> {
        Arc::new(NativeBinding::new_for_test(Arc::clone(mailer), MailboxId(0)))
    }

    fn capture_err(rx: &mpsc::Receiver<EgressEvent>) -> String {
        match decode_reply::<CaptureFrameResult>(rx) {
            CaptureFrameResult::Err { error } => error,
            CaptureFrameResult::Ok { .. } => panic!("expected an Err capture reply"),
        }
    }

    /// Park a capture awaiting two pre-mails; two `pre_settled` mails count
    /// it down to ready, and the next `on_frame` (no GPU) resolves it —
    /// exercising park → `pre_settled`×N → ready-on-frame. Without an
    /// adapter the ready branch fails fast, but the state-machine
    /// transition (countdown then act on the next frame) is what this owns.
    #[test]
    fn park_then_pre_settled_countdown_readies_on_frame() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);
        state.pending_capture = Some(parked_capture(&mailer, None, 2, Instant::now() + FRAME_SETTLEMENT_CAP));
        let binding = ctx_binding(&mailer);

        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        RenderCapability::on_pre_settled(&mut state, &mut ctx, PreSettled { mail_id: MailId::NONE });
        assert_eq!(state.pending_capture.as_ref().expect("still pending").pre_remaining, 1);
        RenderCapability::on_pre_settled(&mut state, &mut ctx, PreSettled { mail_id: MailId::NONE });
        assert!(state.pending_capture.as_ref().expect("still pending").is_ready());

        // A frame past readiness acts on the capture (consumes it).
        RenderCapability::on_frame(&mut state, &mut ctx, Frame { replay_cache_when_idle: false, windows: Vec::new() });
        assert!(state.pending_capture.is_none(), "a ready capture is resolved on the next frame");
        assert!(capture_err(&rx).contains("GPU"), "no adapter in unit tests => the ready branch fails fast");
    }

    /// A capture past its deadline replies `Err` through the retained guard
    /// on the next frame — the `FRAME_SETTLEMENT_CAP` wedge, event-driven.
    #[test]
    fn expired_capture_replies_err_on_frame() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);
        // A deadline in the past, with pre-mails still outstanding.
        let past = Instant::now().checked_sub(Duration::from_secs(1)).expect("clock is past the epoch");
        state.pending_capture = Some(parked_capture(&mailer, None, 3, past));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        RenderCapability::on_frame(&mut state, &mut ctx, Frame { replay_cache_when_idle: false, windows: Vec::new() });

        assert!(state.pending_capture.is_none(), "an expired capture is cleared");
        assert!(capture_err(&rx).contains("settlement cap"), "the wedge disposition replies Err");
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn detached_target_fails_only_its_pending_capture() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);
        state.pending_capture =
            Some(parked_capture(&mailer, Some(WindowId(7)), 1, Instant::now() + FRAME_SETTLEMENT_CAP));

        state.fail_capture_for_detached_window(WindowId(8));
        assert!(state.pending_capture.is_some(), "a different target's capture survives");

        state.fail_capture_for_detached_window(WindowId(7));
        assert!(state.pending_capture.is_none(), "the detached target's capture is cleared");
        assert!(capture_err(&rx).contains("detached"));
    }

    #[test]
    fn surfaceless_capture_selection_is_explicit() {
        let (mailer, _rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);

        assert!(state.validate_capture_target(None).is_err(), "an unconfigured runtime is not implicitly offscreen");
        state.offscreen_size = Some((64, 48));
        assert!(state.validate_capture_target(None).is_ok(), "None explicitly selects the configured offscreen target");
        assert!(state.validate_capture_target(Some(WindowId(3))).is_err(), "unknown window ids stay explicit");
    }

    #[test]
    fn frame_window_ids_are_deduplicated_in_identity_order() {
        assert_eq!(
            deduplicate_windows(vec![WindowId(8), WindowId(2), WindowId(8), WindowId(5)])
                .into_iter()
                .collect::<Vec<_>>(),
            [WindowId(2), WindowId(5), WindowId(8)],
        );
    }

    /// A second `capture_frame` while one is pending replies `Err`
    /// immediately, without disturbing the in-flight capture.
    #[test]
    fn capture_while_pending_replies_err_immediately() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);
        state.offscreen_size = Some((64, 48));
        state.pending_capture = Some(parked_capture(&mailer, None, 1, Instant::now() + FRAME_SETTLEMENT_CAP));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new_dispatching(
            &binding,
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil()))),
            MailId::NONE,
            MailId::NONE,
        );

        RenderCapability::on_capture_frame(
            &mut state,
            &mut ctx,
            CaptureFrame {
                window: None,
                mails: Vec::new(),
                after_mails: Vec::new(),
                checks: Vec::new(),
                similarity: None,
            },
        );

        assert!(state.pending_capture.is_some(), "the in-flight capture is untouched");
        assert!(capture_err(&rx).contains("already pending"), "a second capture is rejected");
    }

    /// Issue #2831: `destroy_texture` removes a user-owned registry entry,
    /// dropping its staged pixels and recording the dispatched kind.
    #[test]
    fn destroy_texture_removes_registry_entry() {
        let (mailer, _rx) = test_mailer_and_rx();
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut state = headless_state(&mailer);
        state.observed_kinds = Some(Arc::clone(&observed));
        let texture_id = 7;
        state.textures.entries.insert(texture_id, test_staged_texture(vec![0xAB; 16]));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        RenderCapability::on_destroy_texture(&mut state, &mut ctx, DestroyTexture { texture_id });

        assert!(
            !state.textures.entries.contains_key(&texture_id),
            "destroy_texture should remove the staged registry entry",
        );
        let seen = observed.lock().expect("observed_kinds mutex is not poisoned").clone();
        assert!(
            seen.contains(&DestroyTexture::NAME.to_owned()),
            "destroy_texture handler should push its kind name; observed: {seen:?}",
        );
    }

    /// Issue #2831: unknown ids and the reserved internal white texture id
    /// warn-drop and leave the registry untouched.
    #[test]
    fn destroy_texture_unknown_and_reserved_ids_leave_registry_untouched() {
        let (mailer, _rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);
        let user_texture_id = 3;
        state.textures.entries.insert(user_texture_id, test_staged_texture(vec![1; 16]));
        state.textures.entries.insert(WHITE_TEXTURE_ID, test_staged_texture(vec![255; 16]));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        for texture_id in [99, WHITE_TEXTURE_ID] {
            RenderCapability::on_destroy_texture(&mut state, &mut ctx, DestroyTexture { texture_id });
        }

        assert_eq!(
            state.textures.entries.len(),
            2,
            "unknown and reserved destroy requests must not remove registry entries",
        );
        assert!(state.textures.entries.contains_key(&user_texture_id));
        assert!(state.textures.entries.contains_key(&WHITE_TEXTURE_ID));
    }

    /// The diagnostic white texture id is visible to `SubstrateHarness` callers but
    /// remains engine-owned: `UpdateTexture` must not recolor later solid
    /// draws through the shared sentinel texel.
    #[test]
    fn update_texture_reserved_id_leaves_white_pixels_untouched() {
        let (mailer, _rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);
        state.textures.entries.insert(WHITE_TEXTURE_ID, test_staged_texture(vec![255; 16]));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        RenderCapability::on_update_texture(
            &mut state,
            &mut ctx,
            UpdateTexture { texture_id: WHITE_TEXTURE_ID, x: 0, y: 0, width: 1, height: 1, pixels: vec![0, 0, 0, 255] },
        );

        assert_eq!(
            state.textures.entries.get(&WHITE_TEXTURE_ID).expect("white texture remains registered").pixels,
            vec![255; 16],
        );
    }

    /// ADR-0107 §4: `draw_solid_quads` accumulates into `quad_frame` under
    /// the reserved `WHITE_TEXTURE_ID` and records its kind name in
    /// `observed_kinds`. Verifies the expand-to-TexturedQuad path and the
    /// lazy white-texture insertion without a GPU.
    #[test]
    fn draw_solid_quads_accumulates_and_observed() {
        let (mailer, _rx) = test_mailer_and_rx();
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut state = headless_state(&mailer);
        state.observed_kinds = Some(Arc::clone(&observed));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        RenderCapability::on_draw_solid_quads(
            &mut state,
            &mut ctx,
            DrawSolidQuads {
                space: QuadSpace::Screen,
                clip: None,
                quads: vec![SolidQuad {
                    x: 10.0,
                    y: 20.0,
                    width: 30.0,
                    height: 40.0,
                    color: Rgba::new(1.0, 0.0, 0.5, 0.8),
                }],
            },
        );

        let seen = observed.lock().expect("observed_kinds mutex is not poisoned").clone();
        assert!(
            seen.contains(&DrawSolidQuads::NAME.to_owned()),
            "draw_solid_quads handler should push its kind name; observed: {seen:?}",
        );

        assert_eq!(state.quad_frame.len(), 1, "one QuadBatch should be in the accumulator");
        assert_eq!(state.quad_frame[0].texture_id, WHITE_TEXTURE_ID, "batch must use the reserved white texture id");
        assert_eq!(state.quad_frame[0].quads.len(), 1, "batch must contain the one expanded quad");
        assert_eq!(
            state.quad_frame[0].quads[0].tint,
            Rgba::new(1.0, 0.0, 0.5, 0.8),
            "expanded quad tint must match the SolidQuad color",
        );
        assert_eq!(state.quad_frame[0].quads[0].width, 30.0);

        let white =
            state.textures.entries.get(&WHITE_TEXTURE_ID).expect("white texture must be lazily inserted on first send");
        assert_eq!(white.format, TextureFormat::Rgba8, "white texture must remain RGBA8");
    }
}
