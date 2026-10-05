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
//!   [`aether_substrate::NativeCtx::subscribe_settlement`], which pushes a
//!   settlement-notice mail from whatever thread the settlement fires on. A
//!   render handler must never block on a pre-mail settlement (the ADR
//!   deadlock: pre-chains terminate back at this mailbox), so the bridge only
//!   mails. `PreSettled` is wire-identical to `aether.trace.settled` (a single
//!   `MailId` field), so the pushed notice decodes as `PreSettled`.
//! - **Capture scoring.** `FrameCheck` verdicts and similarity scoring live in
//!   [`aether_substrate::render::visual`], so the ready-branch readback scores
//!   the verdict and similarity directly.

use crate::runtime::config::parse_clear_color;
use std::collections::BTreeSet;
use std::iter;
use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_actor::{ReplyMode, runtime};
use aether_data::ErasedActorPath;

use aether_kinds::{CaptureFrame, CaptureFrameResult};

use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::chassis::error::BootError;
use aether_substrate::render::visual;
use aether_substrate::render::{
    CaptureMeta, IDENTITY_VIEW_PROJ, MainPassRecord, RenderError, encode_png, map_capture_rgba, prepare_capture_copy,
    record_main_pass, record_resolve_pass,
};
#[cfg(feature = "desktop")]
use winit::window::Window;

// The native impl seams, nested under this `runtime` directory so the one
// `mod runtime;` gate in the parent covers them (no per-sibling `#[cfg]`):
// `pipeline` (GPU bundle + shared record helpers), `texture` (the texture
// registry), `geometry` (the geometry registry), `overlay` (the overlay-batch
// accumulator), `material` (the material-batch accumulator), `capture` (the
// similarity-reference resolver), and `config` (the `RenderTuningConfig`
// knobs + `RenderParams`).
mod capture;
mod config;
pub use config::{DEFAULT_CLEAR_COLOR, apply_manifest_clear_color};
mod device;
// The ADR-0246 draw-set registry: retained lists of draws, each checked
// when its set is made or patched, holding the geometries and instance
// buffers they name.
mod draw_set;
// The ADR-0171 geometry registry: staged vertex/index bytes realized
// lazily as wgpu buffers at first GPU use (the draw-pass slice records
// against the realized side).
mod geometry;
// The hold counts and retired entries a resource registry keeps for the
// draw sets naming its entries (ADR-0246 decision 2).
mod holds;
// The ADR-0246 instance-record registry: fixed-capacity record buffers
// whose CPU copy is the source of truth, written in place on the GPU.
mod instances;
mod material;
// The one accumulator every overlay verb pushes into (ADR-0105 / ADR-0213),
// so painter order inside the overlay pass is receipt order across the three.
mod overlay;
mod pipeline;
// The ADR-0170 authored-render-program registry + executor: register-time
// validation and pipeline construction, dispatch-time resolution and pass
// recording into the frame encoder ahead of the sampling passes.
mod program;
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

use self::pipeline::{OverlayObservation, record_material_batches, record_overlay_batches};
use self::surface::{boot_offscreen, build_wireframe_overlay_pipeline, try_boot_offscreen};
#[cfg(feature = "desktop")]
use self::target::{DesktopGpuContext, FirstWindowGpu, RenderTarget, WindowTargets};

// These seam items are `pub` (visible in `render`) in their now-nested child
// modules, so the re-export up to runtime level keeps that exact visibility.
pub use self::capture::resolve_reference;
use self::capture::{AcceptedCapture, PendingCapture};
use self::device::DeviceRecovery;
pub use self::draw_set::{DrawSet, DrawSetRegistry, DrawSetRows, HeldDraw};
pub use self::geometry::{GeometryRegistry, RealizedGeometry, StagedGeometry};
pub use self::instances::{InstancesRegistry, StagedInstances};
pub use self::material::MaterialBatch;
pub use self::overlay::OverlayBatch;
use self::program::ProgramRegistry;
pub use self::texture::{TextureRegistry, WHITE_TEXTURE_ID};

use super::{
    CreateDrawSet, CreateDrawSetResult, CreateGeometry, CreateGeometryResult, CreateInstances, CreateInstancesResult,
    CreateTexture, CreateTextureResult, DRAW_TRIANGLE_BYTES, DestroyDrawSet, DestroyGeometry, DestroyInstances,
    DestroyTexture, DrawMaterialCoverage, DrawMaterialTextured, DrawScreenTriangles, DrawShapes, DrawTexturedQuads,
    DrawTriangle, Frame, Occluded, PreSettled, ProgramDestroy, ProgramDispatch, ProgramRegister, ProgramRegisterResult,
    ProgramTimings, ProgramTimingsResult, RenderCapability, UpdateDrawSet, UpdateDrawSetResult, UpdateGeometry,
    UpdateInstances, UpdateTexture, ViewProjection,
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
    overlay_frame: Vec<OverlayBatch>,
    overlay_last_submitted: Vec<OverlayBatch>,
    material_frame: Vec<MaterialBatch>,
    material_last_submitted: Vec<MaterialBatch>,
    textures: TextureRegistry,
    /// ADR-0171 geometry resources: the session-scoped registry. Staged
    /// here at create/update; the draw-pass record path realizes and
    /// consumes the wgpu buffers.
    geometries: GeometryRegistry,
    /// ADR-0246 instance records: the session-scoped registry. Its CPU
    /// copy of each buffer is the source of truth; the draw-set record
    /// path realizes and reads the wgpu buffers.
    instances: InstancesRegistry,
    /// ADR-0246 draw sets: the session-scoped registry of retained draw
    /// lists. Each set holds the geometries and instance buffers it
    /// names in the two registries above.
    draw_sets: DrawSetRegistry,
    /// ADR-0170 authored render programs: the session-scoped registry.
    programs: ProgramRegistry,
    /// Dispatches queued since the last frame record. Unlike the draw
    /// accumulators these are one-shot — a program executes once per
    /// dispatch and its output persists in its writable registry
    /// texture — so the record drains this rather than commit/replay.
    pending_program_dispatches: Vec<ProgramDispatch>,
    vertex_buffer_bytes: usize,
    clear_color: wgpu::Color,

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
    /// ADR-0173 generation state and callback bridge for the shared render
    /// device. Kept beside `gpu` so replacement is an actor-owned transaction
    /// rather than a chassis-visible protocol.
    device_recovery: DeviceRecovery,
    wire_pipeline: Option<wgpu::RenderPipeline>,
    /// Prior frame's submission index, drained at the top of the next
    /// frame to bound the present loop to one frame in flight (issue 1312).
    last_submission: Option<wgpu::SubmissionIndex>,
    /// ADR-0161 R4: committed-overlay observation sink for the substrate
    /// harness's `committed_overlay_snapshot`. `record_overlay_batches`
    /// populates it with the quad batches that *survived* record-time
    /// rejection (missing texture / invalid clip / past budget), so the
    /// snapshot reflects what was drawn — not the raw accumulator. Screen
    /// triangle batches are absent by shape: the sink's element is
    /// `DrawTexturedQuads`, and a triangle batch carries neither a quad list
    /// nor a projection to report. `Mutex` only
    /// because `record_overlay_batches` takes `&Mutex<_>` (the harness sink's
    /// shape); the pumped state is single-threaded, so it never contends.
    overlay_observation: Mutex<Vec<DrawTexturedQuads>>,
    /// The shape batches that survived the same record (ADR-0213), kept
    /// beside `overlay_observation` because the sink's element is a quad
    /// batch and a shape batch carries shapes, not quads. Same lifetime,
    /// same reader.
    shape_observation: Mutex<Vec<DrawShapes>>,

    pending_capture: Option<PendingCapture>,

    assets_dir: Option<PathBuf>,
}

struct BuiltReplacement {
    gpu: RenderGpu,
    wire_pipeline: Option<wgpu::RenderPipeline>,
    #[cfg(feature = "desktop")]
    desktop: Option<(WindowTargets<RenderTarget>, DesktopGpuContext)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryTarget {
    Desktop,
    Offscreen((u32, u32)),
    Unavailable,
}

fn select_recovery_target(has_desktop_targets: bool, offscreen_size: Option<(u32, u32)>) -> RecoveryTarget {
    if has_desktop_targets {
        RecoveryTarget::Desktop
    } else if let Some(size) = offscreen_size {
        RecoveryTarget::Offscreen(size)
    } else {
        RecoveryTarget::Unavailable
    }
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

    /// Snapshot the ordered shape batches from the most recently committed
    /// frame as their public [`DrawShapes`] shape (ADR-0213) — the shape
    /// companion of [`Self::committed_overlay_snapshot`], populated by the
    /// same record. Empty until a frame with a recorded shape batch commits.
    ///
    /// # Panics
    ///
    /// If the observation mutex is poisoned (fail-fast per ADR-0063).
    #[must_use]
    pub fn committed_shape_snapshot(&self) -> Vec<DrawShapes> {
        self.shape_observation.lock().expect("mutex poisoned; fail-fast per ADR-0063").clone()
    }

    /// Attach one native window as a render target. The first attachment
    /// selects the adapter/device and builds shared pipelines; later
    /// attachments must support the same copy-compatible color format.
    /// Every fallible operation completes before insertion, so failure leaves
    /// both the target map and shared GPU state unchanged.
    #[cfg(feature = "desktop")]
    pub fn attach_window(&mut self, path: ErasedActorPath, window: Arc<Window>) -> Result<(), String> {
        if self.offscreen_size.is_some() {
            return Err("cannot attach a window target to an explicitly surfaceless render runtime".to_owned());
        }
        let size = window.inner_size();
        let wireframe = self.wireframe.clone();
        let vertex_buffer_bytes = self.vertex_buffer_bytes;

        let install = if let (Some(gpu), Some(context)) = (self.gpu.as_ref(), self.desktop_gpu.as_ref()) {
            let device = Arc::clone(&gpu.device);
            let format = gpu.color_format;
            self.targets.attach_with(path, || {
                RenderTarget::attach_to_booted_gpu(context, &device, window, (size.width, size.height), format)
            })?
        } else if self.gpu.is_none() && self.desktop_gpu.is_none() {
            self.targets.attach_with(path, || {
                RenderTarget::boot_first(window, (size.width, size.height), wireframe.as_deref(), vertex_buffer_bytes)
            })?
        } else {
            return Err("render GPU boot state cannot accept desktop window targets".to_owned());
        };

        if let Some(FirstWindowGpu { context, gpu, wire_pipeline }) = install {
            self.device_recovery.install_initial(&gpu.device);
            self.desktop_gpu = Some(context);
            self.gpu = Some(gpu);
            self.wire_pipeline = wire_pipeline;
        }
        Ok(())
    }

    /// Detach one window surface. A capture selected for that target fails
    /// immediately; captures for other targets and the shared scene survive.
    /// `ctx` is the render actor's own, which answers the failed capture's
    /// held reply.
    #[cfg(feature = "desktop")]
    pub fn detach_window<M: ReplyMode, A>(&mut self, ctx: &mut NativeCtx<'_, A, M>, path: &ErasedActorPath) -> bool {
        let removed = self.targets.detach(path).is_some();
        if removed {
            self.fail_capture_for_detached_window(ctx, path);
        }
        removed
    }

    #[cfg(feature = "desktop")]
    fn fail_capture_for_detached_window<M: ReplyMode, A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        path: &ErasedActorPath,
    ) {
        if self.pending_capture.as_ref().is_some_and(|pending| pending.window.as_ref() == Some(path)) {
            let pending = self.pending_capture.take().expect("just checked Some");
            pending.held.answer(
                ctx,
                &CaptureFrameResult::Err {
                    error: format!("capture_frame failed: window target {path} detached before capture"),
                },
            );
        }
    }

    fn validate_capture_target(&self, window: Option<&ErasedActorPath>) -> Result<(), String> {
        #[cfg(feature = "desktop")]
        {
            if self.targets.validate_capture_selection(window, |target| target.occluded)? {
                return Ok(());
            }
        }
        #[cfg(not(feature = "desktop"))]
        if let Some(window) = window {
            return Err(format!("capture_frame failed: window target {window} is unavailable on this render runtime"));
        }
        if self.offscreen_size.is_some() {
            Ok(())
        } else {
            Err("capture_frame failed: no surfaceless capture target is configured".to_owned())
        }
    }

    /// Accept a `CaptureFrame` up to the point it parks: refuse it while the
    /// device is unusable, prove the requested window once at receipt, check
    /// the target and the one global in-flight limit, prove both bundles,
    /// resolve the similarity reference, and dispatch the pre-mails with
    /// their settlement bridged back here. `Err` is the message the caller
    /// is answered with; nothing has moved when it returns one.
    fn accept_capture<M: ReplyMode>(
        &mut self,
        ctx: &NativeCtx<'_, RenderCapability, M>,
        mail: CaptureFrame,
    ) -> Result<AcceptedCapture, String> {
        self.device_recovery.refresh();
        if let Some(error) = self.device_recovery.unusable_error() {
            return Err(format!("capture_frame failed: {error}"));
        }
        // Keep the canonical path, so a short path selects the same target
        // as the path `aether.window.list` reports (ADR-0166).
        let window = mail.window.as_ref().map(|window| canonical_window(ctx, window)).transpose()?;
        self.validate_capture_target(window.as_ref())?;
        if self.pending_capture.is_some() {
            return Err("capture already pending; try again once the in-flight request completes".to_owned());
        }

        // Prove both bundles before either moves (ADR-0230 §3), so an
        // unprovable recipient in the after bundle aborts before any
        // pre-mail is sent.
        let pre = ctx.accept_bundle(mail.mails, "capture bundle")?;
        let after_mails = ctx.accept_bundle(mail.after_mails, "capture after bundle")?;
        let reference = resolve_reference(self.assets_dir.as_deref(), mail.similarity.as_ref())?;

        // Dispatch each pre-mail on a fresh chassis-rooted chain (issue
        // 860) and bridge its settlement to a `PreSettled` mail addressed
        // to this render mailbox — pushed from whatever thread the
        // settlement fires on. With no settlement registry (some fixtures)
        // `pre_remaining` stays the number dispatched but nothing decrements
        // it, so such a fixture never gates a capture on settlement.
        let mut pre_remaining = 0usize;
        for item in pre {
            let mail_id = ctx.deliver_detached(item);
            pre_remaining += 1;
            let _ = ctx.subscribe_settlement::<PreSettled>(mail_id);
        }

        Ok(AcceptedCapture {
            window,
            after_mails,
            checks: mail.checks,
            reference,
            pre_remaining,
            deadline: Instant::now() + FRAME_SETTLEMENT_CAP,
        })
    }

    /// Boot the explicit surfaceless harness GPU. Desktop GPUs are booted by
    /// `attach_window`, never by a frame or a shared handle.
    fn ensure_offscreen_gpu_booted(&mut self) {
        if self.gpu.is_some() || !self.device_recovery.is_unbooted() {
            return;
        }
        let Some((width, height)) = self.offscreen_size else {
            return;
        };
        let booted = boot_offscreen(self.wireframe.as_deref());
        let (gpu, wire_pipeline) = self.build_offscreen_gpu(
            Arc::clone(&booted.device),
            Arc::clone(&booted.queue),
            booted.format,
            (width, height),
            booted.polygon_mode,
            booted.build_overlay,
        );
        self.device_recovery.install_initial(&gpu.device);
        self.wire_pipeline = wire_pipeline;
        self.gpu = Some(gpu);
    }

    fn build_offscreen_gpu(
        &self,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        polygon_mode: wgpu::PolygonMode,
        build_overlay: bool,
    ) -> (RenderGpu, Option<wgpu::RenderPipeline>) {
        let gpu =
            RenderGpu::new(Arc::clone(&device), queue, format, size.0, size.1, polygon_mode, self.vertex_buffer_bytes);
        let wire_pipeline = build_overlay
            .then(|| build_wireframe_overlay_pipeline(&device, gpu.color_format, &gpu.pipeline.pipeline_layout));
        (gpu, wire_pipeline)
    }

    fn build_replacement_gpu(&self) -> Result<BuiltReplacement, String> {
        #[cfg(feature = "desktop")]
        let has_desktop_targets = !self.targets.is_empty();
        #[cfg(not(feature = "desktop"))]
        let has_desktop_targets = false;

        match select_recovery_target(has_desktop_targets, self.offscreen_size) {
            RecoveryTarget::Desktop => {
                #[cfg(feature = "desktop")]
                {
                    let (targets, FirstWindowGpu { context, gpu, wire_pipeline }) =
                        RenderTarget::build_replacement_targets(
                            &self.targets,
                            self.wireframe.as_deref(),
                            self.vertex_buffer_bytes,
                        )?;
                    Ok(BuiltReplacement { gpu, wire_pipeline, desktop: Some((targets, context)) })
                }
                #[cfg(not(feature = "desktop"))]
                unreachable!("desktop recovery requires the desktop feature")
            }
            RecoveryTarget::Offscreen((width, height)) => {
                let booted = try_boot_offscreen(self.wireframe.as_deref())?;
                let (gpu, wire_pipeline) = self.build_offscreen_gpu(
                    Arc::clone(&booted.device),
                    Arc::clone(&booted.queue),
                    booted.format,
                    (width, height),
                    booted.polygon_mode,
                    booted.build_overlay,
                );
                Ok(BuiltReplacement {
                    gpu,
                    wire_pipeline,
                    #[cfg(feature = "desktop")]
                    desktop: None,
                })
            }
            RecoveryTarget::Unavailable => {
                Err("device replacement requires an offscreen target or retained desktop windows".to_owned())
            }
        }
    }

    /// Drain loss notices and, when needed, perform the lost generation's
    /// one replacement transaction. Offscreen targets or the complete
    /// canonical desktop target map are built off to the side; registry
    /// realizations are then switched in the same actor-owned commit. A
    /// failed device or surface acquisition is terminal.
    fn recover_gpu_if_needed<M: ReplyMode, A>(&mut self, ctx: &mut NativeCtx<'_, A, M>) -> Result<(), String> {
        self.device_recovery.refresh();
        if let Some(error) = self.device_recovery.unusable_error() {
            return Err(error);
        }
        let Some(ticket) = self.device_recovery.begin_replacement() else {
            return Ok(());
        };

        let BuiltReplacement {
            gpu,
            wire_pipeline,
            #[cfg(feature = "desktop")]
            desktop,
        } = match self.build_replacement_gpu() {
            Ok(replacement) => replacement,
            Err(reason) => {
                self.finish_failed_replacement(ctx, ticket, reason.clone());
                return Err(reason);
            }
        };

        self.textures.invalidate_device_resources();
        self.geometries.invalidate_device_resources();
        self.instances.invalidate_device_resources();
        self.programs.rebuild_for_device(&gpu);
        self.discard_device_replay_caches();
        self.device_recovery.finish_replacement(ticket, &gpu.device);
        self.last_submission = None;
        #[cfg(feature = "desktop")]
        if let Some((targets, context)) = desktop {
            self.targets = targets;
            self.desktop_gpu = Some(context);
        }
        self.wire_pipeline = wire_pipeline;
        self.gpu = Some(gpu);
        Ok(())
    }

    fn finish_failed_replacement<M: ReplyMode, A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        ticket: device::ReplacementTicket,
        reason: String,
    ) {
        self.last_submission = None;
        self.wire_pipeline = None;
        self.gpu = None;
        self.device_recovery.fail_replacement(ticket, reason.clone());
        self.fail_pending_capture_for_device(ctx, format!("capture_frame failed: {reason}"));
    }

    /// Reject request/reply GPU work while loss is pending or terminal.
    /// Replacement belongs to the first frame after a known loss, keeping
    /// acquisition out of ordinary mail handlers.
    fn service_device_for_request(&mut self) -> Result<(), String> {
        self.device_recovery.refresh();
        self.device_recovery.gpu_work_error().map_or(Ok(()), Err)
    }

    fn warn_drop_if_unusable(&mut self, operation: &'static str) -> bool {
        self.device_recovery.refresh();
        let Some(error) = self.device_recovery.unusable_error() else {
            return false;
        };
        tracing::warn!(
            target: "aether_render",
            operation,
            %error,
            "dropping fire-and-forget render work because the render capability is unusable",
        );
        true
    }

    fn fail_pending_capture_for_device<M: ReplyMode, A>(&mut self, ctx: &mut NativeCtx<'_, A, M>, error: String) {
        let Some(pending) = self.pending_capture.take() else {
            return;
        };
        for item in pending.after_mails {
            let _ = ctx.deliver_detached(item);
        }
        pending.held.answer(ctx, &CaptureFrameResult::Err { error });
    }

    /// Host-only deterministic injection reached through the concrete
    /// `GpuFrameHook`. No kind, wire shape, or actor callback is exposed.
    pub fn force_device_loss_for_harness(&self) -> Result<u64, String> {
        let gpu = self.gpu.as_ref().ok_or_else(|| "the healthy render device has no published GPU".to_owned())?;
        let generation = self.device_recovery.force_current_loss()?;
        gpu.device.destroy();
        Ok(generation)
    }

    fn commit_scene(&mut self, replay_cache_when_idle: bool) {
        commit_or_replay(&mut self.frame_vertices, &mut self.last_submitted, replay_cache_when_idle);
        commit_or_replay(&mut self.material_frame, &mut self.material_last_submitted, replay_cache_when_idle);
        commit_or_replay(&mut self.overlay_frame, &mut self.overlay_last_submitted, replay_cache_when_idle);
    }

    /// Drop only scene caches that may have been submitted ambiguously on
    /// the lost generation. Live accumulators and pending program dispatches
    /// are known not to have recorded yet and survive for the replacement
    /// frame's ordinary commit.
    fn discard_device_replay_caches(&mut self) {
        discard_replay_cache(&mut self.last_submitted);
        discard_replay_cache(&mut self.material_last_submitted);
        discard_replay_cache(&mut self.overlay_last_submitted);
    }

    /// Record the world / material / overlay passes into `encoder` from the
    /// already-committed global scene. The caller may invoke it once per
    /// dirty target at that target's dimensions without consuming the scene
    /// again.
    fn record_passes(&mut self, encoder: &mut wgpu::CommandEncoder) -> Result<(), RenderError> {
        let gpu = self.gpu.as_ref().expect("record_passes requires a booted GPU");
        // Authored program passes first (ADR-0170): their outputs are
        // registry textures the material and overlay passes below sample,
        // so a dispatch and a draw over its output land in one frame.
        let dispatches = mem::take(&mut self.pending_program_dispatches);
        self.programs.record(gpu, encoder, &mut self.textures, &mut self.geometries, &dispatches);
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
                encoder,
                MainPassRecord {
                    queue: &gpu.queue,
                    pipeline: &gpu.pipeline,
                    targets: &targets,
                    vertices: &self.last_submitted,
                    view_proj: &self.camera_state,
                    extra_pipelines: extras,
                    clear: self.clear_color,
                },
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
                &self.overlay_last_submitted,
                self.camera_state,
                Some(OverlayObservation { quads: &self.overlay_observation, shapes: &self.shape_observation }),
            );
        }
        // Every pass above rasterized into the multisampled pair; resolve
        // once, here at the end of the chain, into the single-sample
        // texture the swapchain blit and the capture readback consume.
        {
            let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            record_resolve_pass(encoder, &targets);
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
        // The timing readback is mapped only once its copy is submitted;
        // the map completes on a later frame's poll, so nothing here
        // waits on it (iamacoffeepot/aether#4423).
        self.programs.after_frame_submit();
        if let Some(texture) = surface_texture {
            texture.present();
        }
        Ok(capture_meta)
    }

    fn complete_capture<M: ReplyMode, A>(&mut self, ctx: &mut NativeCtx<'_, A, M>, meta: CaptureMeta) {
        let pending = self.pending_capture.take().expect("capture metadata requires a pending capture");
        for item in pending.after_mails {
            let _ = ctx.deliver_detached(item);
        }
        let gpu = self.gpu.as_ref().expect("capture metadata requires a booted GPU");
        let readback = {
            let targets = gpu.targets.lock().expect("mutex poisoned; fail-fast per ADR-0063");
            map_capture_rgba(&gpu.device, &targets, &meta)
        };
        if let Err(error) = &readback {
            self.device_recovery.report_current_loss(format!("capture mapping/readback failed: {error}"));
        } else {
            self.device_recovery.refresh();
        }
        let outcome: Result<CaptureFrameResult, String> = self.device_recovery.gpu_work_error().map_or_else(
            || {
                readback.and_then(|rgba| {
                    let png = encode_png(&rgba, meta.width, meta.height)?;
                    let (similarity_score, similarity_pass) =
                        visual::score_similarity(&rgba, meta.width, meta.height, pending.reference.as_ref())?;
                    let verdict = (!pending.checks.is_empty())
                        .then(|| visual::run_checks(rgba, meta.width, meta.height, &pending.checks));
                    Ok(CaptureFrameResult::Ok { png, verdict, similarity_score, similarity_pass })
                })
            },
            |error| Err(format!("capture_frame failed during device loss: {error}")),
        );
        let result = outcome.unwrap_or_else(|error| CaptureFrameResult::Err { error });
        pending.held.answer(ctx, &result);
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

fn discard_replay_cache<T>(last: &mut Vec<T>) {
    last.clear();
}

/// The canonical path of the live actor `window` names, or the capture error
/// naming `window` when it does not prove.
fn canonical_window<M: ReplyMode, A>(
    ctx: &NativeCtx<'_, A, M>,
    window: &ErasedActorPath,
) -> Result<ErasedActorPath, String> {
    ctx.resolve_path(window)
        .map(|target| ctx.actor_path(target))
        .map_err(|error| format!("capture_frame failed: window {window} does not resolve: {error}"))
}

fn deduplicate_windows(windows: Vec<ErasedActorPath>) -> BTreeSet<ErasedActorPath> {
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
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<RenderCapabilityState, BootError> {
        Ok(RenderCapabilityState {
            frame_vertices: Vec::with_capacity(config.vertex_buffer_bytes),
            last_submitted: Vec::with_capacity(config.vertex_buffer_bytes),
            triangles_rendered: 0,
            camera_state: IDENTITY_VIEW_PROJ,
            overlay_frame: Vec::new(),
            overlay_last_submitted: Vec::new(),
            material_frame: Vec::new(),
            material_last_submitted: Vec::new(),
            textures: TextureRegistry::new(),
            geometries: GeometryRegistry::new(),
            instances: InstancesRegistry::new(),
            draw_sets: DrawSetRegistry::new(),
            programs: ProgramRegistry::new(config.pass_timings),
            pending_program_dispatches: Vec::new(),
            vertex_buffer_bytes: config.vertex_buffer_bytes,
            clear_color: {
                let [r, g, b] = parse_clear_color(&config.clear_color);
                wgpu::Color { r, g, b, a: 1.0 }
            },
            #[cfg(feature = "desktop")]
            targets: WindowTargets::default(),
            #[cfg(feature = "desktop")]
            desktop_gpu: None,
            offscreen_size: params.offscreen_size,
            wireframe: params.wireframe,
            gpu: None,
            device_recovery: DeviceRecovery::new(),
            wire_pipeline: None,
            last_submission: None,
            overlay_observation: Mutex::new(Vec::new()),
            shape_observation: Mutex::new(Vec::new()),
            pending_capture: None,
            assets_dir: params.assets_dir,
        })
    }

    /// `DrawTriangle` accumulator, on the owned `frame_vertices` buffer.
    /// Truncates at the cap boundary, rounding to whole triangles.
    #[handler::tell]
    fn on_draw_triangle(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mails: &[DrawTriangle]) {
        if state.warn_drop_if_unusable("draw_triangle") {
            return;
        }
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
    #[handler::tell]
    fn on_camera(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ViewProjection) {
        if state.warn_drop_if_unusable("view_projection") {
            return;
        }
        state.camera_state = mail.view_proj;
    }

    /// `CreateTexture` (ADR-0105), on the owned texture registry.
    #[handler::request]
    fn on_create_texture(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CreateTexture,
    ) -> CreateTextureResult {
        if let Err(error) = state.service_device_for_request() {
            return CreateTextureResult::Err { error };
        }
        state.textures.create(mail)
    }

    /// `UpdateTexture` (ADR-0105), on the owned texture registry.
    #[handler::tell]
    fn on_update_texture(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UpdateTexture) {
        if state.warn_drop_if_unusable("update_texture") {
            return;
        }
        state.textures.update(mail);
    }

    /// `DestroyTexture`, on the owned texture registry.
    #[handler::tell]
    fn on_destroy_texture(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DestroyTexture) {
        if state.warn_drop_if_unusable("destroy_texture") {
            return;
        }
        state.textures.destroy(mail);
    }

    /// `CreateGeometry` (ADR-0171), on the owned geometry registry —
    /// validation and id assignment are CPU-side, so the reply needs no
    /// booted GPU; the buffers realize lazily at first GPU use.
    #[handler::request]
    fn on_create_geometry(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CreateGeometry,
    ) -> CreateGeometryResult {
        if let Err(error) = state.service_device_for_request() {
            return CreateGeometryResult::Err { error };
        }
        state.geometries.create(mail)
    }

    /// `UpdateGeometry` (ADR-0171), on the owned geometry registry.
    #[handler::tell]
    fn on_update_geometry(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UpdateGeometry) {
        if state.warn_drop_if_unusable("update_geometry") {
            return;
        }
        state.geometries.update(mail);
    }

    /// `DestroyGeometry` (ADR-0171), on the owned geometry registry —
    /// mirrors `destroy_texture`.
    #[handler::tell]
    fn on_destroy_geometry(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DestroyGeometry) {
        if state.warn_drop_if_unusable("destroy_geometry") {
            return;
        }
        state.geometries.destroy(mail);
    }

    /// `CreateInstances` (ADR-0246), on the owned instance registry —
    /// validation, the record copy and id assignment are CPU-side, so
    /// the reply needs no booted GPU.
    #[handler::request]
    fn on_create_instances(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CreateInstances,
    ) -> CreateInstancesResult {
        if let Err(error) = state.service_device_for_request() {
            return CreateInstancesResult::Err { error };
        }
        state.instances.create(mail)
    }

    /// `UpdateInstances` (ADR-0246), on the owned instance registry.
    #[handler::tell]
    fn on_update_instances(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UpdateInstances) {
        if state.warn_drop_if_unusable("update_instances") {
            return;
        }
        state.instances.update(mail);
    }

    /// `DestroyInstances` (ADR-0246), on the owned instance registry.
    #[handler::tell]
    fn on_destroy_instances(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DestroyInstances) {
        if state.warn_drop_if_unusable("destroy_instances") {
            return;
        }
        state.instances.destroy(mail);
    }

    /// `CreateDrawSet` (ADR-0246), on the owned draw-set registry —
    /// every draw is checked against the geometry and instance
    /// registries before the reply.
    #[handler::request]
    fn on_create_draw_set(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CreateDrawSet,
    ) -> CreateDrawSetResult {
        if let Err(error) = state.service_device_for_request() {
            return CreateDrawSetResult::Err { error };
        }
        state.draw_sets.create(mail, &mut state.geometries, &mut state.instances)
    }

    /// `UpdateDrawSet` (ADR-0246), on the owned draw-set registry. A
    /// refused patch replies its reason and changes nothing.
    #[handler::request]
    fn on_update_draw_set(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: UpdateDrawSet,
    ) -> UpdateDrawSetResult {
        if let Err(error) = state.service_device_for_request() {
            return UpdateDrawSetResult::Err { error };
        }
        state.draw_sets.update(mail, &mut state.geometries, &mut state.instances)
    }

    /// `DestroyDrawSet` (ADR-0246), on the owned draw-set registry —
    /// lets go of every buffer the set held.
    #[handler::tell]
    fn on_destroy_draw_set(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DestroyDrawSet) {
        if state.warn_drop_if_unusable("destroy_draw_set") {
            return;
        }
        state.draw_sets.destroy(mail, &mut state.geometries, &mut state.instances);
    }

    /// `ProgramRegister` (ADR-0170): validate the WGSL and pass graph,
    /// build every pass pipeline under a wgpu validation error scope, and
    /// reply the assigned session-scoped `program_id` — or the failing
    /// check's distinguishable `Err` message. Pipeline construction needs a
    /// live device, so the offscreen GPU boots here if configured; on
    /// desktop a register before the first window attaches replies `Err`
    /// rather than parking.
    #[handler::request]
    fn on_program_register(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ProgramRegister,
    ) -> ProgramRegisterResult {
        state.ensure_offscreen_gpu_booted();
        if let Err(error) = state.service_device_for_request() {
            return ProgramRegisterResult::Err { error };
        }
        let Some(gpu) = state.gpu.as_ref() else {
            return ProgramRegisterResult::Err {
                error: "the render GPU is not booted; register programs after the first window attaches".to_owned(),
            };
        };
        state.programs.register(gpu, mail)
    }

    /// `ProgramDispatch` (ADR-0170): queue one execution for the next
    /// frame record. One-shot — the program's output persists in its
    /// writable registry texture, so nothing replays. Runtime mismatches
    /// warn-drop at record time, naming program, pass, and binding.
    #[handler::tell]
    fn on_program_dispatch(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ProgramDispatch) {
        if state.warn_drop_if_unusable("program_dispatch") {
            return;
        }
        state.pending_program_dispatches.push(mail);
    }

    /// `ProgramDestroy` (ADR-0170), on the owned program registry —
    /// mirrors `destroy_texture`.
    #[handler::tell]
    fn on_program_destroy(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ProgramDestroy) {
        if state.warn_drop_if_unusable("program_destroy") {
            return;
        }
        state.programs.destroy(&mail);
    }

    /// `ProgramTimings` (iamacoffeepot/aether#4423): the per-pass GPU
    /// duration table one registered program has accumulated. Reads
    /// already-folded state — the measurement itself resolves a frame
    /// later off the frame's critical path — so the reply costs a walk
    /// of the graph and never touches the device.
    #[handler::request]
    fn on_program_timings(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ProgramTimings,
    ) -> ProgramTimingsResult {
        if let Err(error) = state.service_device_for_request() {
            return ProgramTimingsResult::Err { error };
        }
        state.programs.timings(&mail)
    }

    /// `DrawTexturedQuads` accumulator (ADR-0105), on the owned `overlay_frame`.
    #[handler::tell]
    fn on_draw_textured_quads(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawTexturedQuads) {
        if state.warn_drop_if_unusable("draw_textured_quads") {
            return;
        }
        state.overlay_frame.push(OverlayBatch::textured(mail));
    }

    /// `DrawScreenTriangles` (iamacoffeepot/aether#5504), on the owned
    /// `overlay_frame` — arbitrary pixel-space triangles on the overlay pass,
    /// so flat 2D content keeps its proportions on a non-square window
    /// without a camera publishing a projection for it.
    #[handler::tell]
    fn on_draw_screen_triangles(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawScreenTriangles) {
        if state.warn_drop_if_unusable("draw_screen_triangles") {
            return;
        }
        let batch = OverlayBatch::screen_triangles(mail, &mut state.textures);
        state.overlay_frame.push(batch);
    }

    /// `DrawShapes` (ADR-0213), on the owned `overlay_frame` — rounded,
    /// stroked, shadowed boxes evaluated as a distance field on the overlay
    /// pass, at the same painter position as the quad batches.
    #[handler::tell]
    fn on_draw_shapes(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawShapes) {
        if state.warn_drop_if_unusable("draw_shapes") {
            return;
        }
        state.overlay_frame.push(OverlayBatch::shapes(mail));
    }

    /// `DrawMaterialTextured` (ADR-0140), on the owned material stream.
    #[handler::tell]
    fn on_draw_material_textured(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawMaterialTextured) {
        if state.warn_drop_if_unusable("draw_material_textured") {
            return;
        }
        state.material_frame.push(MaterialBatch::textured(mail));
    }

    /// `DrawMaterialCoverage` (ADR-0140), on the owned material stream.
    #[handler::tell]
    fn on_draw_material_coverage(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrawMaterialCoverage) {
        if state.warn_drop_if_unusable("draw_material_coverage") {
            return;
        }
        state.material_frame.push(MaterialBatch::coverage(mail));
    }

    /// `PreSettled` (ADR-0161) — decrement the pending capture's
    /// `pre_remaining`. A stray notice with no pending capture is ignored.
    /// Engine-only mail (ADR-0233).
    #[handler::event]
    fn on_pre_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: PreSettled) {
        if let Some(pending) = &mut state.pending_capture {
            pending.pre_remaining = pending.pre_remaining.saturating_sub(1);
        }
    }

    /// `Occluded` — update only the named target and fail only a capture
    /// selected for that target. Engine-only mail (ADR-0233).
    #[handler::tell]
    fn on_occluded(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Occluded) {
        #[cfg(feature = "desktop")]
        let became_occluded =
            state.targets.set_occluded(&mail.window, mail.occluded, |target, occluded| target.occluded = occluded)
                && mail.occluded;
        #[cfg(not(feature = "desktop"))]
        let became_occluded = false;

        if became_occluded
            && state.pending_capture.as_ref().is_some_and(|pending| pending.window.as_ref() == Some(&mail.window))
        {
            let pending = state.pending_capture.take().expect("just checked Some");
            pending.held.answer(
                ctx,
                &CaptureFrameResult::Err {
                    error: format!(
                        "capture_frame failed: window target {} became occluded before capture",
                        mail.window
                    ),
                },
            );
        }
    }

    /// `Frame` commits the application-scoped scene once, deduplicates its
    /// dirty window paths, then records and presents that committed scene at
    /// each live non-occluded target's dimensions. A target whose record
    /// fails drops that target's frame and nothing else — the fan-out still
    /// owes every window behind it its turn. An empty target list is
    /// reserved for the explicitly surfaceless harness. Engine-only mail
    /// (ADR-0233).
    #[handler::tell]
    fn on_frame(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Frame) {
        let Frame { replay_cache_when_idle, windows } = mail;
        let windows = deduplicate_windows(windows);

        // Deadline disposition first — a wedged pre-chain replies `Err`
        // even on a frame where nothing else happens.
        let now = Instant::now();
        if state.pending_capture.as_ref().is_some_and(|pending| pending.is_expired(now)) {
            let pending = state.pending_capture.take().expect("just checked Some");
            pending.held.answer(
                ctx,
                &CaptureFrameResult::Err {
                    error: "capture_frame failed: pre-mail settlement did not complete within the frame settlement cap"
                        .to_owned(),
                },
            );
        }

        state.ensure_offscreen_gpu_booted();
        if state.recover_gpu_if_needed(ctx).is_err() {
            return;
        }
        let Some(gpu) = state.gpu.as_ref() else {
            if state.pending_capture.as_ref().is_some_and(PendingCapture::is_ready)
                && let Some(pending) = state.pending_capture.take()
            {
                pending.held.answer(
                    ctx,
                    &CaptureFrameResult::Err {
                        error: "capture_frame failed: the render GPU is not booted on this chassis".to_owned(),
                    },
                );
            }
            return;
        };
        let device = Arc::clone(&gpu.device);

        // One-frame-in-flight: drain the prior submission before recording
        // any target in the next global frame (issue 1312).
        if let Some(index) = state.last_submission.take()
            && let Err(error) = device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None })
        {
            state.device_recovery.report_current_loss(format!("waiting for the previous frame failed: {error}"));
        }
        // The poll may itself deliver the callback. A pending capture has
        // not begun recording this frame, so it may survive a successful
        // transaction and record exactly once below.
        if state.recover_gpu_if_needed(ctx).is_err() {
            return;
        }
        state.commit_scene(replay_cache_when_idle);
        #[cfg(feature = "desktop")]
        let device = Arc::clone(&state.gpu.as_ref().expect("recovery published a GPU").device);

        #[cfg(feature = "desktop")]
        for window in &windows {
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
                .is_some_and(|pending| pending.is_ready() && pending.window.as_ref() == Some(window));
            let meta = match state.record_target_frame(width, height, surface_texture, capture) {
                Ok(meta) => meta,
                // A record failure disposes of *this* target's frame only —
                // the fan-out owes every listed window its turn, so the loop
                // moves on instead of abandoning the ones behind it.
                Err(RenderError::VertexBufferOverflow { vertex_bytes, cap }) => {
                    tracing::warn!(
                        target: "aether_substrate::render",
                        %window,
                        vertex_bytes,
                        cap,
                        "dropping this window's frame: vertex bytes exceed the buffer; remaining windows still present",
                    );
                    continue;
                }
            };
            if let Some(meta) = meta {
                state.complete_capture(ctx, meta);
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
                state.complete_capture(ctx, meta);
            }
        }
    }

    /// `CaptureFrame` — canonicalize and validate the explicit
    /// desktop/offscreen selection, enforce the one global in-flight limit,
    /// then park the mail-driven
    /// capture state machine until the selected target's next dirty frame.
    ///
    /// A render handler must **never** block on a pre-mail settlement (the
    /// ADR Context deadlock: pre-chains terminate back at this mailbox), so
    /// the settlement bridge only mails — it never waits.
    ///
    /// Every exit answers the captured caller through the held ticket, the
    /// same edge the deferred readback replies through: an early `Err`
    /// answers it before the handler returns, and an accepted capture parks
    /// it for the frame loop. The failure paths once answered through the
    /// hub outbound instead, which routes only `Session` / `EngineMailbox`
    /// senders and drops a `Component` one — and an RPC `Call` names the rpc
    /// server's own mailbox as its reply target, so every rejected capture
    /// over the wire returned no image, no error and no timeout
    /// (iamacoffeepot/aether#4341).
    #[handler::request]
    fn on_capture_frame(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: CaptureFrame,
    ) -> Pending<CaptureFrameResult> {
        let (pending, held) = ctx.hold::<CaptureFrameResult>();
        match state.accept_capture(ctx, mail) {
            Ok(accepted) => state.pending_capture = Some(accepted.park(held)),
            Err(error) => held.answer(ctx, &CaptureFrameResult::Err { error }),
        }
        pending
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
    use super::super::{ScreenTriangle, ScreenVertex, Shape, TextureFormat, TextureSampling, TextureUsage};
    use super::texture::{StagedTexture, TexturePixels};
    use super::*;
    use aether_actor::HandlesKind;
    use aether_data::{Blob, Kind, SessionToken, Uuid};
    use aether_kinds::QuadSpace;
    use aether_math::Rgba;
    use aether_substrate::chassis::builder::ReplyTarget;
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::testing::{
        PumpedDriver, boot_bare_test_chassis, decode_session_reply, fresh_substrate_and_rx,
    };
    use std::sync::mpsc::Receiver;

    fn window(name: &str) -> ErasedActorPath {
        ErasedActorPath::new(&format!("aether.window/aether.window.instance:{name}")).expect("fixture window path")
    }

    fn test_staged_texture(pixels: Vec<u8>) -> StagedTexture {
        StagedTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: TexturePixels::Received(Blob::from(pixels)),
            realized: None,
            dirty: true,
        }
    }

    /// A minimal headless state for the state tests — no window, no GPU
    /// (`gpu` stays `None`, so nothing touches an absent adapter).
    fn headless_state() -> RenderCapabilityState {
        RenderCapabilityState {
            frame_vertices: Vec::new(),
            last_submitted: Vec::new(),
            triangles_rendered: 0,
            camera_state: IDENTITY_VIEW_PROJ,
            overlay_frame: Vec::new(),
            overlay_last_submitted: Vec::new(),
            material_frame: Vec::new(),
            material_last_submitted: Vec::new(),
            textures: TextureRegistry::new(),
            geometries: GeometryRegistry::new(),
            instances: InstancesRegistry::new(),
            draw_sets: DrawSetRegistry::new(),
            programs: ProgramRegistry::new(false),
            pending_program_dispatches: Vec::new(),
            vertex_buffer_bytes: 1024,
            clear_color: wgpu::Color { r: 0.05, g: 0.07, b: 0.12, a: 1.0 },
            #[cfg(feature = "desktop")]
            targets: WindowTargets::default(),
            #[cfg(feature = "desktop")]
            desktop_gpu: None,
            offscreen_size: None,
            wireframe: None,
            gpu: None,
            device_recovery: DeviceRecovery::new(),
            wire_pipeline: None,
            last_submission: None,
            overlay_observation: Mutex::new(Vec::new()),
            shape_observation: Mutex::new(Vec::new()),
            pending_capture: None,
            assets_dir: None,
        }
    }

    /// A booted `aether.render` on a pumped slot, the home production
    /// drives it from, with no GPU: nothing here sends a frame that would
    /// boot one. Every mail reaches the cap through the chassis and runs
    /// through production dispatch when the slot drains; replies go to a
    /// session on the loopback egress.
    struct RenderFixture {
        cap: PumpedDriver<RenderCapability>,
        egress: Receiver<EgressEvent>,
    }

    impl RenderFixture {
        fn boot(params: RenderParams) -> Self {
            let (registry, mailer, egress) = fresh_substrate_and_rx();
            let chassis = boot_bare_test_chassis(&registry, &mailer);
            let tuning = RenderTuningConfig {
                vertex_buffer_bytes: 1024,
                clear_color: DEFAULT_CLEAR_COLOR.to_owned(),
                pass_timings: false,
            };
            let cap = PumpedDriver::boot(chassis, tuning, params);
            Self { cap, egress }
        }

        /// Seed the texture registry with `texture_id` in a host turn: a
        /// `create_texture` would need a device to realize it.
        fn with_texture(&mut self, texture_id: u32, pixels: Vec<u8>) {
            self.cap
                .host_turn(|state, _ctx| {
                    state.textures.entries.insert(texture_id, test_staged_texture(pixels));
                })
                .expect("the booted slot takes a host turn");
        }

        /// Deliver `mail` to the cap as a tracked chassis root, its reply
        /// (if any) routed to `reply`, and pump the slot until that root's
        /// chain settles.
        fn deliver<K: Kind>(&mut self, mail: &K, reply: Option<ReplyTarget>)
        where
            RenderCapability: HandlesKind<K>,
        {
            self.cap.send_and_settle(self.cap.chassis().actor_ref::<RenderCapability>(), mail, reply);
        }

        fn send<K: Kind>(&mut self, mail: &K)
        where
            RenderCapability: HandlesKind<K>,
        {
            self.deliver(mail, None);
        }

        /// [`Self::deliver`] with the reply routed to a session, decoded.
        fn request<K: Kind, R: Kind>(&mut self, mail: &K) -> R
        where
            RenderCapability: HandlesKind<K>,
        {
            let reply = ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0x7045)), correlation: 1 };
            self.deliver(mail, Some(reply));
            decode_session_reply(&self.egress)
        }

        fn read<T>(&self, read: impl FnOnce(&RenderCapabilityState) -> T) -> T {
            self.cap.read_state(read).expect("the slot is live")
        }
    }

    #[test]
    fn surfaceless_capture_selection_is_explicit() {
        let mut state = headless_state();

        assert!(state.validate_capture_target(None).is_err(), "an unconfigured runtime is not implicitly offscreen");
        state.offscreen_size = Some((64, 48));
        assert!(state.validate_capture_target(None).is_ok(), "None explicitly selects the configured offscreen target");
        assert!(state.validate_capture_target(Some(&window("main"))).is_err(), "unknown windows stay explicit");
    }

    #[test]
    fn frame_windows_are_deduplicated_in_path_order() {
        assert_eq!(
            deduplicate_windows(vec![window("h"), window("b"), window("h"), window("e")])
                .into_iter()
                .collect::<Vec<_>>(),
            [window("b"), window("e"), window("h")],
        );
    }

    #[test]
    fn recovery_target_prefers_retained_desktop_state_then_offscreen() {
        assert_eq!(
            select_recovery_target(true, Some((64, 48))),
            RecoveryTarget::Desktop,
            "retained windows own the replacement adapter selection",
        );
        assert_eq!(select_recovery_target(false, Some((64, 48))), RecoveryTarget::Offscreen((64, 48)));
        assert_eq!(select_recovery_target(false, None), RecoveryTarget::Unavailable);
    }

    #[test]
    fn replacement_discards_only_old_replay_cache_before_committing_live_work() {
        let mut state = headless_state();
        state.last_submitted = vec![1, 2, 3];
        state.frame_vertices = vec![4, 5, 6];
        state.pending_program_dispatches.push(ProgramDispatch {
            program_id: 9,
            bindings: Vec::new(),
            geometries: Vec::new(),
            uniforms: Vec::new(),
        });

        state.discard_device_replay_caches();

        assert!(state.last_submitted.is_empty(), "ambiguously submitted replay cache is discarded");
        assert_eq!(state.frame_vertices, [4, 5, 6], "fresh frame mail survives replacement");
        assert_eq!(state.pending_program_dispatches[0].program_id, 9, "fresh program dispatch survives replacement");

        state.commit_scene(true);
        assert_eq!(state.last_submitted, [4, 5, 6], "the replacement frame commits fresh work, not the old cache");
    }

    /// Catches a terminal device that retries acquisition, or a mail shape
    /// that slips past the unusable gate: request/reply work and captures
    /// answer the terminal error, and fire-and-forget updates and draws drop.
    #[test]
    fn terminal_device_failure_never_reboots_and_disposes_each_mail_shape() {
        let mut render =
            RenderFixture::boot(RenderParams { offscreen_size: Some((64, 48)), ..RenderParams::default() });
        render.with_texture(3, vec![7; 16]);
        render
            .cap
            .host_turn(|state, _ctx| state.device_recovery.force_unusable_for_test("replacement acquisition failed"))
            .expect("the slot is live");

        render.send(&Frame { replay_cache_when_idle: true, windows: Vec::new() });
        let registered: ProgramRegisterResult = render.request(&ProgramRegister {
            wgsl: String::new(),
            bindings: Vec::new(),
            transients: Vec::new(),
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: Vec::new(),
        });
        render.send(&UpdateTexture { texture_id: 3, x: 0, y: 0, width: 1, height: 1, pixels: vec![9, 9, 9, 9] });
        render.send(&DrawTriangle::default());
        let captured: CaptureFrameResult = render.request(&CaptureFrame {
            window: None,
            mails: Vec::new(),
            after_mails: Vec::new(),
            checks: Vec::new(),
            similarity: None,
        });

        assert!(
            matches!(registered, ProgramRegisterResult::Err { ref error } if error.contains("unusable")),
            "request/reply GPU work returns the terminal structured error: {registered:?}",
        );
        assert!(
            matches!(captured, CaptureFrameResult::Err { ref error } if error.contains("unusable")),
            "capture is refused with the terminal structured error: {captured:?}",
        );
        render.read(|state| {
            assert!(state.gpu.is_none(), "terminal state never retries device acquisition");
            assert!(state.pending_capture.is_none(), "a refused capture parks nothing");
            assert_eq!(state.textures.entries[&3].pixels.bytes(), vec![7; 16], "fire-and-forget updates are dropped");
            assert!(state.frame_vertices.is_empty(), "fire-and-forget draws are dropped");
        });
    }

    /// Issue #2831. Catches a `destroy_texture` that leaves a user-owned
    /// registry entry, and its staged pixels, resident.
    #[test]
    fn destroy_texture_removes_registry_entry() {
        let mut render = RenderFixture::boot(RenderParams::default());
        render.with_texture(7, vec![0xAB; 16]);

        render.send(&DestroyTexture { texture_id: 7 });

        assert!(
            !render.read(|state| state.textures.entries.contains_key(&7)),
            "destroy_texture should remove the staged registry entry",
        );
    }

    /// Issue #2831. Catches a `destroy_texture` that removes the reserved
    /// internal white texture, or disturbs the registry for an unknown id:
    /// both warn-drop.
    #[test]
    fn destroy_texture_unknown_and_reserved_ids_leave_registry_untouched() {
        let mut render = RenderFixture::boot(RenderParams::default());
        render.with_texture(3, vec![1; 16]);
        render.with_texture(WHITE_TEXTURE_ID, vec![255; 16]);

        for texture_id in [99, WHITE_TEXTURE_ID] {
            render.send(&DestroyTexture { texture_id });
        }

        render.read(|state| {
            assert_eq!(state.textures.entries.len(), 2, "unknown and reserved destroys must not remove entries");
            assert!(state.textures.entries.contains_key(&3));
            assert!(state.textures.entries.contains_key(&WHITE_TEXTURE_ID));
        });
    }

    /// Catches an `UpdateTexture` that recolors the engine-owned white
    /// texture: its id is visible to `SubstrateHarness` callers, and every
    /// later solid draw samples its shared texel.
    #[test]
    fn update_texture_reserved_id_leaves_white_pixels_untouched() {
        let mut render = RenderFixture::boot(RenderParams::default());
        render.with_texture(WHITE_TEXTURE_ID, vec![255; 16]);

        render.send(&UpdateTexture {
            texture_id: WHITE_TEXTURE_ID,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
        });

        assert_eq!(
            render.read(|state| state.textures.entries[&WHITE_TEXTURE_ID].pixels.bytes().to_vec()),
            vec![255; 16]
        );
    }

    /// ADR-0213. Catches `draw_shapes` accumulating anywhere but the one
    /// overlay accumulator, which would break painter order against the
    /// other overlay verbs, and a first solid send that leaves the reserved
    /// white texture uninserted.
    #[test]
    fn draw_shapes_accumulates_in_painter_order() {
        let mut render = RenderFixture::boot(RenderParams::default());
        let corner = |x: f32, y: f32| ScreenVertex { x, y, color: Rgba::WHITE };
        let shape = Shape {
            x: 10.0,
            y: 20.0,
            width: 30.0,
            height: 40.0,
            corner_radius: 4.0,
            fill: Some(Rgba::WHITE),
            stroke: None,
            shadow: None,
            texture: None,
        };

        render.send(&DrawScreenTriangles {
            space: QuadSpace::Screen,
            clip: None,
            triangles: vec![ScreenTriangle { a: corner(0.0, 0.0), b: corner(8.0, 0.0), c: corner(4.0, 8.0) }],
        });
        render.send(&DrawShapes { space: QuadSpace::Screen, clip: None, shapes: vec![shape.clone()] });

        render.read(|state| {
            assert_eq!(state.overlay_frame.len(), 2, "both batches share the one overlay accumulator");
            let OverlayBatch::Shapes { shapes, .. } = &state.overlay_frame[1] else {
                panic!("a shape submission must accumulate as a shape batch, after the triangles sent before it");
            };
            assert_eq!(shapes.as_slice(), &[shape]);
            let white = state
                .textures
                .entries
                .get(&WHITE_TEXTURE_ID)
                .expect("white texture must be lazily inserted on first send");
            assert_eq!(white.format, TextureFormat::Rgba8, "white texture must remain RGBA8");
        });
    }
}
