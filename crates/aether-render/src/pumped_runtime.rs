//! The pumped `aether.render` runtime (ADR-0161 slice R2). The
//! state-bearing companion to the pooled [`super::RenderCapability`],
//! added *beside* it: the pooled GPU runtime stays intact so the desktop
//! driver (R3) and the substrate harness (R4) swap onto this one
//! independently, and every intermediate commit ships green. The pooled
//! runtime is deleted in R5.
//!
//! Where the pooled runtime hands its accumulators to a separate driver
//! thread through `RenderHandles`' nine `Arc<Mutex<…>>` fields plus a
//! cross-thread `CaptureBackend`, this runtime owns every accumulator as a
//! plain field and the GPU + pending capture outright: it is a *pumped*
//! actor (ADR-0160), dispatched on the chassis driver thread, so frame
//! recording, capture readback, and present all run on the one thread that
//! owns the surface. The three chassis-internal kinds ([`Frame`],
//! [`PreSettled`], [`Occluded`]) turn frame invocation, pre-mail
//! settlement, and window occlusion into mail — so every capture
//! transition is a handler with trace brackets and a cost row, and the
//! capture state machine is testable headlessly with a toy pump.
//!
//! Gated on the `runtime` feature: ADR-0161 R4 lifts the runtime off
//! `desktop` so the substrate harness builds the **offscreen** (surfaceless)
//! pumped path without winit — the windowed boot inside is `desktop`-gated
//! line by line. The pooled runtime and headless companion pay nothing.
//!
//! ## Capture bridge notes (ADR-0161):
//! - **Settlement bridge.** Per the R2 hand-off, the capture pre-settlement
//!   path deliberately keeps the mail-push bridge rather than R1's
//!   callback-form `subscribe_settlement`:
//!   [`on_capture_frame`](PumpedRenderCapability::on_capture_frame) bridges
//!   each pre-mail settlement to a [`PreSettled`] mail through
//!   [`SettlementRegistry::subscribe_settlement_mail`], which pushes a
//!   settlement-notice mail from whatever thread the settlement fires on. A
//!   render handler must never block on a pre-mail settlement (the ADR
//!   deadlock: pre-chains terminate back at this mailbox), so the bridge only
//!   mails. `PreSettled` is wire-identical to `aether.trace.settled` (a single
//!   `MailId` field), so the pushed notice decodes as `PreSettled`. The
//!   advance-loop wait, by contrast, uses R1's `PumpWake` /
//!   `await_settlement_pumped` on the driver side — the two are not unified.
//! - **Capture scoring.** `FrameCheck` verdicts and similarity scoring live in
//!   [`aether_substrate::render::visual`] (ADR-0161 R3 rehomed the scorer
//!   below this crate), so the ready-branch readback scores the verdict and
//!   similarity directly — `aether-harness-substrate-capture` re-exports the
//!   same module for its own asserts.

use std::io;
use std::iter;
use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_actor::runtime;
use aether_data::{Kind, MailId};
use serde::{Deserialize, Serialize};

use aether_kinds::{CaptureFrame, CaptureFrameResult, FrameCheck};

use aether_substrate::Manual;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::capture::ReferenceCapture;
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::inbox::InboundMail;
use aether_substrate::mail::Mail;
use aether_substrate::mail::helpers::resolve_bundle;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use aether_substrate::render::visual;
use aether_substrate::render::{
    IDENTITY_VIEW_PROJ, RenderError, encode_png, map_capture_rgba, prepare_capture_copy, record_main_pass,
};

use crate::runtime::{
    MaterialBatch, QuadBatch, RenderGpu, StagedTexture, TextureRegistry, WHITE_TEXTURE_ID, acquire_surface_texture,
    boot_offscreen, build_wireframe_overlay_pipeline, expected_pixel_bytes, record_material_batches,
    record_overlay_batches,
};
// Winit-window boot (ADR-0161 R4: the offscreen harness path boots
// surfaceless via `boot_offscreen` and never touches a winit window).
#[cfg(feature = "desktop")]
use crate::runtime::{WindowCell, boot_surface};

use super::runtime::resolve_reference;
use super::{
    CreateTexture, CreateTextureResult, DRAW_TRIANGLE_BYTES, DestroyTexture, DrawMaterialCoverage,
    DrawMaterialTextured, DrawSolidQuads, DrawTexturedQuads, DrawTriangle, PumpedRenderCapability, PumpedRenderParams,
    RenderTuningConfig, SolidQuad, TextureFormat, TexturedQuad, UpdateTexture, ViewProjection,
};

/// Wedge-to-`Err` cap for a parked capture (ADR-0161): if a capture's
/// pre-mail chain has not settled within this window the next frame past
/// the deadline replies `Err`, reproducing the pooled
/// `FRAME_SETTLEMENT_CAP` disposition event-driven. Matches the desktop
/// driver's 30s advance-settlement bound.
const FRAME_SETTLEMENT_CAP: Duration = Duration::from_secs(30);

/// Chassis-internal frame-request kind (ADR-0161 §Decision 1). The driver
/// mails one each `RedrawRequested` after the advance chain settles;
/// `PumpedRenderCapability::on_frame` records the frame (and resolves any
/// pending capture). `replay_cache_when_idle` carries the issue 847
/// semantic — harness captures replay the last committed accumulators when
/// the producer was idle; desktop always commits current.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.render.frame")]
pub struct Frame {
    pub replay_cache_when_idle: bool,
}

/// Chassis-internal pre-mail-settlement notice (ADR-0161 §Decision 4). One
/// arrives per capture pre-mail whose causal chain has settled;
/// `PumpedRenderCapability::on_pre_settled` decrements the pending
/// capture's `pre_remaining`. Wire-identical to `aether.trace.settled` (a
/// single `MailId` field) so the settlement registry's notice-mail bridge
/// (`subscribe_settlement_mail`) delivers it directly.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.render.pre_settled")]
pub struct PreSettled {
    pub mail_id: MailId,
}

/// Chassis-internal window-occlusion signal (ADR-0161 §Decision 4). The
/// driver forwards `WindowEvent::Occluded`;
/// `PumpedRenderCapability::on_occluded` fail-fasts a pending capture
/// when the window becomes occluded (relocating `fail_capture_if_occluded`
/// into the actor, issue 1317).
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.render.occluded")]
pub struct Occluded {
    pub occluded: bool,
}

/// A parked capture, as plain owned state (ADR-0161 §Decision 4) — no
/// `Arc`, no atomic, no cross-thread queue. The retained [`InboundMail`]
/// guard defers the reply a frame (or more) past `on_capture_frame`; its
/// un-fired `record_finished` keeps the inbound's chain open until the
/// reply lands (ADR-0080 §6, ADR-0106).
struct PendingCapture {
    reply: InboundMail,
    after_mails: Vec<Mail>,
    /// `FrameCheck` verdict requests, scored on the read-back RGBA in
    /// `on_frame`'s ready-branch (ADR-0161 §Decision 4). Since R3 rehomed the
    /// scorer into `aether_substrate::render::visual`, the branch is reachable
    /// without the `aether-harness-substrate-capture` cycle R2 documented.
    checks: Vec<FrameCheck>,
    /// Optional similarity reference (issue 1780), scored alongside `checks`.
    reference: Option<ReferenceCapture>,
    /// Count of pre-mail settlements still awaited; `on_pre_settled`
    /// decrements it, and `on_frame` captures once it reaches zero.
    pre_remaining: usize,
    /// Wall-clock instant past which the capture wedges to `Err`.
    deadline: Instant,
}

impl PendingCapture {
    /// Ready to read back — every pre-mail chain has settled.
    fn is_ready(&self) -> bool {
        self.pre_remaining == 0
    }

    /// Past its wedge deadline (`FRAME_SETTLEMENT_CAP` since parking).
    fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

/// Pumped `aether.render` runtime state (ADR-0161). Owns the accumulators,
/// the GPU + surface, and the pending capture as plain fields. The
/// addressing identity is the distinct ZST [`super::PumpedRenderCapability`].
pub struct PumpedRenderCapabilityState {
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

    /// Shared late-bound window handle; the first `on_frame` after the
    /// chassis fills it boots wgpu. `None` until params provide it.
    /// Desktop-only: the offscreen harness path (`offscreen_size`) owns no
    /// window (ADR-0161 R4).
    #[cfg(feature = "desktop")]
    window: Option<WindowCell>,
    /// ADR-0161 R4: offscreen boot dimensions. `Some((w, h))` makes the
    /// first `on_frame` (with no window cell filled) boot a surfaceless GPU
    /// at these dimensions — the substrate harness's path.
    offscreen_size: Option<(u32, u32)>,
    /// Resolved `AETHER_WIREFRAME` value threaded from params.
    wireframe: Option<String>,
    /// wgpu bundle, booted lazily on the first `on_frame` with a filled
    /// window cell (`get_or_insert_with`).
    gpu: Option<RenderGpu>,
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,
    wire_pipeline: Option<wgpu::RenderPipeline>,
    /// Prior frame's submission index, drained at the top of the next
    /// frame to bound the present loop to one frame in flight (issue 1312).
    last_submission: Option<wgpu::SubmissionIndex>,
    /// Whether the window is currently occluded (issue 1317).
    occluded: bool,

    /// ADR-0161 R4: committed-overlay observation sink for the substrate
    /// harness's `committed_overlay_snapshot`. `record_overlay_batches`
    /// populates it with the batches that *survived* record-time rejection
    /// (missing texture / invalid clip / past budget), so the snapshot
    /// reflects what was drawn — not the raw accumulator. `Mutex` only
    /// because `record_overlay_batches` takes `&Mutex<_>` (the pooled sink's
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

impl PumpedRenderCapabilityState {
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

    /// Whether a capture is currently parked (ADR-0161 R4). Read by the
    /// substrate harness's frame hook through `PumpedSlot::read_state` to
    /// decide whether to drive a frame — the pumped state's fields are
    /// private, so this is the read surface the pump-owning thread uses.
    #[must_use]
    pub fn has_pending_capture(&self) -> bool {
        self.pending_capture.is_some()
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

    /// Boot wgpu against the shared window cell if it is now filled and the
    /// GPU is not yet booted. Idempotent — the one-time boot lands visibly
    /// in the first frame's cost sample (ADR-0161). No-op until the chassis
    /// `resumed` handler fills the cell.
    fn ensure_gpu_booted(&mut self) {
        if self.gpu.is_some() {
            return;
        }
        // Windowed path (desktop): boot against the shared window cell once
        // the chassis's `resumed` fills it, standing up a surface + swapchain.
        #[cfg(feature = "desktop")]
        if let Some(window) = self.window.as_ref().and_then(|cell| cell.get().cloned()) {
            let size = window.inner_size();
            let booted = boot_surface(window, (size.width, size.height), self.wireframe.as_deref());
            let gpu = RenderGpu::new(
                Arc::clone(&booted.device),
                Arc::clone(&booted.queue),
                booted.format,
                booted.config.width,
                booted.config.height,
                booted.polygon_mode,
                self.vertex_buffer_bytes,
            );
            // Built post-`RenderGpu::new` so it can borrow the main pipeline's
            // layout (same camera bind group). The overlay draws into the
            // offscreen color target, so it uses `RenderGpu`'s color format.
            self.wire_pipeline = booted.build_overlay.then(|| {
                build_wireframe_overlay_pipeline(&booted.device, gpu.color_format, &gpu.pipeline.pipeline_layout)
            });
            self.surface = Some(booted.surface);
            self.surface_config = Some(booted.config);
            self.gpu = Some(gpu);
            return;
        }
        // Offscreen path (ADR-0161 R4, the substrate harness): boot a
        // surfaceless GPU at the configured dimensions. No surface / swapchain
        // — capture reads back from the offscreen targets directly, and the
        // best-effort present in `on_frame` no-ops with `surface: None`.
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

    /// Reconfigure the surface + offscreen targets when the window has
    /// resized since the last frame (ADR-0161: the surface reconfigure rides
    /// the next `on_frame` rather than a driver reach-in — the R2 pumped
    /// shape carries no size on the frame mail). No-op until the GPU is
    /// booted, when the size is unchanged, or on a `0×0` minimize (winit
    /// reports `Resized(0, 0)` there, which `Targets::resize` also guards).
    /// Desktop-only: the offscreen harness path owns no window to resize
    /// against (ADR-0161 R4).
    #[cfg(feature = "desktop")]
    fn reconfigure_if_resized(&mut self) {
        let Some(window) = self.window.as_ref().and_then(|cell| cell.get().cloned()) else {
            return;
        };
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let Some(config) = self.surface_config.as_mut() else {
            return;
        };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 || (size.width == config.width && size.height == config.height) {
            return;
        }
        config.width = size.width;
        config.height = size.height;
        if let Some(surface) = self.surface.as_ref() {
            surface.configure(&gpu.device, config);
        }
        gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063").resize(
            &gpu.device,
            size.width,
            size.height,
        );
    }

    /// Record the world / material / overlay passes into `encoder` from the
    /// owned accumulators, committing each per the issue 847 cache
    /// semantic. Returns `Err` if the vertex buffer overflowed (the frame
    /// is dropped). The GPU must be booted.
    fn record_passes(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        replay_cache_when_idle: bool,
    ) -> Result<(), RenderError> {
        // Commit every accumulator first (mutating owned fields) so the
        // shared GPU borrow taken for recording never overlaps a `&mut`
        // on a sibling field.
        commit_or_replay_owned(&mut self.frame_vertices, &mut self.last_submitted, replay_cache_when_idle);
        commit_or_replay_owned(&mut self.material_frame, &mut self.material_last_submitted, replay_cache_when_idle);
        commit_or_replay_owned(&mut self.quad_frame, &mut self.quad_last_submitted, replay_cache_when_idle);

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
}

/// Owned-field commit-or-replay (ADR-0161 §Scope: "a bare `mem::swap`
/// decision"). Mirrors the pooled `commit_or_replay` on `Mutex<Vec<T>>`:
/// - `live` non-empty → swap it into `last` and clear `live` for next frame.
/// - `live` empty, `!replay_cache_when_idle` → clear `last` (commit-current).
/// - `live` empty, `replay_cache_when_idle` → leave `last` (replay-cache).
fn commit_or_replay_owned<T>(live: &mut Vec<T>, last: &mut Vec<T>, replay_cache_when_idle: bool) {
    if !live.is_empty() {
        mem::swap(live, last);
        live.clear();
    } else if !replay_cache_when_idle {
        last.clear();
    }
}

#[runtime]
impl NativeActor for PumpedRenderCapability {
    type State = PumpedRenderCapabilityState;
    type Config = RenderTuningConfig;
    type Params = PumpedRenderParams;

    const NAMESPACE: &'static str = "aether.render";

    fn init(
        config: RenderTuningConfig,
        params: PumpedRenderParams,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<PumpedRenderCapabilityState, BootError> {
        let mailer = ctx.mailer();
        let registry = Arc::clone(mailer.registry());
        let outbound = mailer.outbound().cloned().ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "HubOutbound must be wired on Mailer before PumpedRenderCapability::init",
            )))
        })?;
        Ok(PumpedRenderCapabilityState {
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
            window: params.window,
            offscreen_size: params.offscreen_size,
            wireframe: params.wireframe,
            gpu: None,
            surface: None,
            surface_config: None,
            wire_pipeline: None,
            last_submission: None,
            occluded: false,
            overlay_observation: Mutex::new(Vec::new()),
            pending_capture: None,
            registry,
            mailer,
            outbound,
            assets_dir: params.assets_dir,
            observed_kinds: params.observed_kinds,
        })
    }

    /// `DrawTriangle` accumulator, on the owned `frame_vertices` buffer
    /// (the pooled handler with the mutex removed). Truncates at the cap
    /// boundary, rounding to whole triangles.
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
        let Some(expected) = expected_pixel_bytes(mail.width, mail.height, mail.format) else {
            return CreateTextureResult::Err {
                error: format!("texture dimensions {}x{} overflow or are zero", mail.width, mail.height),
            };
        };
        if mail.pixels.len() != expected {
            return CreateTextureResult::Err {
                error: format!(
                    "pixels length {} does not match {}x{} {:?} = {expected}",
                    mail.pixels.len(),
                    mail.width,
                    mail.height,
                    mail.format
                ),
            };
        }
        let texture_id = state.textures.next_id;
        state.textures.next_id += 1;
        state.textures.entries.insert(
            texture_id,
            StagedTexture {
                width: mail.width,
                height: mail.height,
                format: mail.format,
                pixels: mail.pixels,
                realized: None,
                dirty: true,
            },
        );
        CreateTextureResult::Ok { texture_id }
    }

    /// `UpdateTexture` (ADR-0105), on the owned texture registry.
    #[handler::single]
    fn on_update_texture(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UpdateTexture) {
        state.observe(<UpdateTexture as Kind>::NAME);
        if mail.texture_id == WHITE_TEXTURE_ID {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "update_texture for reserved internal texture id; dropping",
            );
            return;
        }
        let Some(entry) = state.textures.entries.get_mut(&mail.texture_id) else {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "update_texture for unknown texture id; dropping",
            );
            return;
        };
        if !entry.apply_subrect(mail.x, mail.y, mail.width, mail.height, &mail.pixels) {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "update_texture rect out of bounds, zero-sized, or pixel length mismatch; \
                 dropping",
            );
        }
    }

    /// `DestroyTexture`, on the owned texture registry.
    #[handler::single]
    fn on_destroy_texture(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DestroyTexture) {
        state.observe(<DestroyTexture as Kind>::NAME);
        if mail.texture_id == WHITE_TEXTURE_ID {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "destroy_texture for reserved internal texture id; dropping",
            );
            return;
        }
        if state.textures.entries.remove(&mail.texture_id).is_none() {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "destroy_texture for unknown texture id; dropping",
            );
        }
    }

    /// `DrawTexturedQuads` accumulator (ADR-0105), on the owned `quad_frame`.
    #[handler::single]
    fn on_draw_textured_quads(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawTexturedQuads) {
        state.observe(<DrawTexturedQuads as Kind>::NAME);
        state.quad_frame.push(QuadBatch {
            texture_id: mail.texture_id,
            space: mail.space,
            clip: mail.clip,
            quads: mail.quads,
        });
    }

    /// `DrawSolidQuads` (ADR-0107 §4), on the owned `quad_frame` — expand to
    /// the reserved white texture tinted by `color`.
    #[handler::single]
    fn on_draw_solid_quads(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawSolidQuads) {
        state.observe(<DrawSolidQuads as Kind>::NAME);
        state.textures.entries.entry(WHITE_TEXTURE_ID).or_insert_with(|| StagedTexture {
            width: 1,
            height: 1,
            format: TextureFormat::Rgba8,
            pixels: vec![255, 255, 255, 255],
            realized: None,
            dirty: true,
        });
        let quads: Vec<TexturedQuad> = mail
            .quads
            .into_iter()
            .map(|SolidQuad { x, y, width, height, color }| TexturedQuad {
                x,
                y,
                width,
                height,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: color,
            })
            .collect();
        state.quad_frame.push(QuadBatch { texture_id: WHITE_TEXTURE_ID, space: mail.space, clip: mail.clip, quads });
    }

    /// `DrawMaterialTextured` (ADR-0140), on the owned material stream.
    #[handler::single]
    fn on_draw_material_textured(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawMaterialTextured) {
        state.observe(<DrawMaterialTextured as Kind>::NAME);
        state.material_frame.push(MaterialBatch::Textured { texture_id: mail.texture_id, rects: mail.rects });
    }

    /// `DrawMaterialCoverage` (ADR-0140), on the owned material stream.
    #[handler::single]
    fn on_draw_material_coverage(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawMaterialCoverage) {
        state.observe(<DrawMaterialCoverage as Kind>::NAME);
        state.material_frame.push(MaterialBatch::Coverage { texture_id: mail.texture_id, rects: mail.rects });
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

    /// `Occluded` (ADR-0161) — track window occlusion and fail-fast a
    /// pending capture the moment the window becomes occluded (issue 1317
    /// `fail_capture_if_occluded`, relocated into the actor). The reply
    /// rides the retained guard, so the inbound's chain settles after it.
    #[handler::single]
    fn on_occluded(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Occluded) {
        state.observe(<Occluded as Kind>::NAME);
        state.occluded = mail.occluded;
        if mail.occluded
            && let Some(pending) = state.pending_capture.take()
        {
            pending.reply.reply(&CaptureFrameResult::Err {
                error: "capture_frame failed: the window became occluded before the frame could be captured".to_owned(),
            });
        }
    }

    /// `Frame` (ADR-0161 §Decision 1) — the per-frame record + capture
    /// decision. Boots wgpu lazily on the first frame with a filled window
    /// cell, records the world / material / overlay passes, and resolves a
    /// pending capture: past its deadline → reply `Err`; ready
    /// (`pre_remaining == 0`) → read back + reply through the retained
    /// guard; otherwise the capture rides a later frame.
    #[handler::single]
    fn on_frame(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Frame) {
        state.observe(<Frame as Kind>::NAME);
        state.ensure_gpu_booted();
        // Desktop-only: the offscreen harness path owns no window to resize.
        #[cfg(feature = "desktop")]
        state.reconfigure_if_resized();

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

        let capture_this_frame = state.pending_capture.as_ref().is_some_and(PendingCapture::is_ready);

        let Some(gpu) = state.gpu.as_ref() else {
            // No surface yet (window cell unfilled / boot skipped). A ready
            // capture cannot read back without a GPU — fail it fast rather
            // than park it forever.
            if capture_this_frame && let Some(pending) = state.pending_capture.take() {
                pending.reply.reply(&CaptureFrameResult::Err {
                    error: "capture_frame failed: the render GPU is not booted on this chassis".to_owned(),
                });
            }
            return;
        };
        let device = Arc::clone(&gpu.device);
        let queue = Arc::clone(&gpu.queue);

        // One-frame-in-flight: drain the prior submission before recording
        // the next (issue 1312).
        if let Some(index) = state.last_submission.take()
            && let Err(error) = device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None })
        {
            tracing::warn!(target: "aether_substrate::render", ?error, "device.poll for previous frame failed; continuing");
        }

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame encoder") });
        match state.record_passes(&mut encoder, mail.replay_cache_when_idle) {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => return,
        }

        // Capture copy against the offscreen — independent of surface
        // availability.
        let capture_meta = capture_this_frame.then(|| {
            let gpu = state.gpu.as_ref().expect("gpu present in this branch");
            let mut targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            prepare_capture_copy(&gpu.device, &mut targets, &mut encoder)
        });

        // Best-effort blit to the swapchain + present.
        let surface_tex = state
            .surface
            .as_ref()
            .zip(state.surface_config.as_ref())
            .and_then(|(surface, config)| acquire_surface_texture(surface, &device, config));
        if let Some(tex) = surface_tex.as_ref() {
            let gpu = state.gpu.as_ref().expect("gpu present in this branch");
            let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            let (width, height) = (targets.width(), targets.height());
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: targets.color_texture(),
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
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }

        state.last_submission = Some(queue.submit(iter::once(encoder.finish())));
        if let Some(tex) = surface_tex {
            tex.present();
        }

        // Capture readback + deferred reply through the retained guard.
        if let Some(meta) = capture_meta {
            let pending = state.pending_capture.take().expect("capture_this_frame => pending is Some");
            for mail in pending.after_mails {
                state.mailer.push(mail);
            }
            let gpu = state.gpu.as_ref().expect("gpu present in this branch");
            // Map the readback once, then encode the PNG and score the
            // verdict / similarity from the same de-padded RGBA (ADR-0161
            // §Decision 4 restores the pooled path's parity). `score_similarity`
            // borrows the slice; `run_checks` consumes it, so the similarity
            // check runs first (issue 1780).
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

    /// `CaptureFrame` (ADR-0161 §Decision 4) — the mail-driven capture
    /// entry. Fails fast when the window is occluded or a capture is
    /// already pending (issue 1317 semantics preserved at the source),
    /// then resolves the pre / after bundles, dispatches each pre-mail on a
    /// fresh chassis-rooted chain, bridges its settlement to a `PreSettled`
    /// mail, parks the [`PendingCapture`], and retains the inbound guard so
    /// the reply defers to `on_frame`.
    ///
    /// A render handler must **never** block on a pre-mail settlement (the
    /// ADR Context deadlock: pre-chains terminate back at this mailbox), so
    /// the settlement bridge only mails — it never waits.
    #[handler::manual]
    fn on_capture_frame(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: CaptureFrame) {
        state.observe(<CaptureFrame as Kind>::NAME);
        let sender = ctx.reply_target();

        if state.occluded {
            state.outbound.send_reply(
                sender,
                &CaptureFrameResult::Err { error: "capture_frame failed: the window is occluded".to_owned() },
            );
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
        // settlement fires on (the R1 callback-form subscribe is not yet
        // landed; `subscribe_settlement_mail` is the mail-push case of it).
        // With no settlement registry (some fixtures) `pre_remaining` stays
        // the number dispatched but nothing decrements it, so such a
        // fixture never gates a capture on settlement — matching the pooled
        // runtime's behaviour on those fixtures.
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
    /// natural reader is this actor's `unwire` rather than a driver
    /// shutdown path.
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
    use super::*;
    use aether_data::{KindId, MailboxId, Source, SourceAddr};
    use aether_data::{SessionToken, Uuid};
    use aether_kinds::trace::Nanos;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::chassis::inbox::SettlingInbox;
    use aether_substrate::mail::registry::OwnedDispatch;
    use aether_substrate::mail::{EgressEvent, MailRef};
    use aether_substrate::testing::{decode_reply, test_mailer_and_rx};
    use std::sync::mpsc;

    /// Build a `PendingCapture` whose retained guard replies to a Session
    /// source so the toy pump can observe the deferred reply through the
    /// egress channel. The inbound is queued straight onto a
    /// `SettlingInbox` (no route), then drained to the guard.
    fn parked_capture(mailer: &Arc<Mailer>, pre_remaining: usize, deadline: Instant) -> PendingCapture {
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
        PendingCapture { reply, after_mails: Vec::new(), checks: Vec::new(), reference: None, pre_remaining, deadline }
    }

    /// A minimal headless state for the capture state-machine tests — no
    /// window, no GPU (`gpu` stays `None`, so the ready branch fails fast
    /// rather than touching an absent adapter).
    fn headless_state(mailer: &Arc<Mailer>) -> PumpedRenderCapabilityState {
        let outbound = mailer.outbound().cloned().expect("test_mailer_and_rx wires a loopback outbound");
        PumpedRenderCapabilityState {
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
            window: None,
            offscreen_size: None,
            wireframe: None,
            gpu: None,
            surface: None,
            surface_config: None,
            wire_pipeline: None,
            last_submission: None,
            occluded: false,
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
        state.pending_capture = Some(parked_capture(&mailer, 2, Instant::now() + FRAME_SETTLEMENT_CAP));
        let binding = ctx_binding(&mailer);

        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        PumpedRenderCapability::on_pre_settled(&mut state, &mut ctx, PreSettled { mail_id: MailId::NONE });
        assert_eq!(state.pending_capture.as_ref().expect("still pending").pre_remaining, 1);
        PumpedRenderCapability::on_pre_settled(&mut state, &mut ctx, PreSettled { mail_id: MailId::NONE });
        assert!(state.pending_capture.as_ref().expect("still pending").is_ready());

        // A frame past readiness acts on the capture (consumes it).
        PumpedRenderCapability::on_frame(&mut state, &mut ctx, Frame { replay_cache_when_idle: false });
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
        state.pending_capture = Some(parked_capture(&mailer, 3, past));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        PumpedRenderCapability::on_frame(&mut state, &mut ctx, Frame { replay_cache_when_idle: false });

        assert!(state.pending_capture.is_none(), "an expired capture is cleared");
        assert!(capture_err(&rx).contains("settlement cap"), "the wedge disposition replies Err");
    }

    /// The window becoming occluded while a capture is pending fail-fasts it
    /// through the guard (issue 1317, relocated into the actor).
    #[test]
    fn occlusion_while_pending_replies_err() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);
        state.pending_capture = Some(parked_capture(&mailer, 1, Instant::now() + FRAME_SETTLEMENT_CAP));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        PumpedRenderCapability::on_occluded(&mut state, &mut ctx, Occluded { occluded: true });

        assert!(state.pending_capture.is_none(), "occlusion clears the pending capture");
        assert!(state.occluded, "occlusion state is tracked");
        assert!(capture_err(&rx).contains("occluded"), "occlusion fail-fasts the capture");
    }

    /// A second `capture_frame` while one is pending replies `Err`
    /// immediately, without disturbing the in-flight capture.
    #[test]
    fn capture_while_pending_replies_err_immediately() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut state = headless_state(&mailer);
        state.pending_capture = Some(parked_capture(&mailer, 1, Instant::now() + FRAME_SETTLEMENT_CAP));
        let binding = ctx_binding(&mailer);
        let mut ctx = NativeCtx::new_dispatching(
            &binding,
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil()))),
            MailId::NONE,
            MailId::NONE,
        );

        PumpedRenderCapability::on_capture_frame(
            &mut state,
            &mut ctx,
            CaptureFrame { mails: Vec::new(), after_mails: Vec::new(), checks: Vec::new(), similarity: None },
        );

        assert!(state.pending_capture.is_some(), "the in-flight capture is untouched");
        assert!(capture_err(&rx).contains("already pending"), "a second capture is rejected");
    }
}
