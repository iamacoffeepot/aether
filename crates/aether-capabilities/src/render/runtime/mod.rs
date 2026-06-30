//! The `aether.render` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "render-runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a
//! marker-only `render` build of the [`RenderCapability`]
//! identity never names these types nor pulls the wgpu-bound substrate
//! runtime through this cap. The substrate-typed imports + GPU-bound
//! helpers are gated once by this module rather than line-by-line; the
//! `#[actor] impl` reaches the state, ctx, and accumulator helpers through
//! the single `use runtime::*` glob in the parent.

// `Arc` is named here only by the state struct's field types; the parent
// `#[actor] impl` gets its own `Arc` from the shared `any(render-runtime,
// runtime)` import in `mod.rs`, so this stays a private import to avoid a
// redundant re-export. The substrate ctx types (`NativeActor` / `NativeCtx`
// / `NativeInitCtx` / `BootError` / `Manual` / `CaptureFrameResult`) the
// `#[actor] impl` names come from that same shared seam, not from here.
use std::sync::Arc;

pub(super) use std::sync::atomic::{AtomicU64, Ordering};
pub(super) use std::sync::{Mutex, OnceLock};

pub(super) use aether_data::Kind;
pub(super) use aether_substrate::capture::PendingCapture;
pub(super) use aether_substrate::mail::helpers::resolve_bundle;
pub(super) use aether_substrate::mail::mailer::Mailer;
pub(super) use aether_substrate::mail::registry::Registry;
pub(super) use aether_substrate::render::IDENTITY_VIEW_PROJ;

// The native impl seams, now nested under this `runtime` directory so the one
// `mod runtime;` gate in the parent covers them (no per-sibling `#[cfg]`):
// `pipeline` (GPU bundle + accumulator handles), `texture` (the texture
// registry), `quad` (the quad-batch accumulator), `capture` (the cross-thread
// readback machinery), and `config` (the per-instance `RenderConfig`).
mod capture;
mod config;
mod pipeline;
mod quad;
mod texture;

// The cap-root re-exports source these four names through `runtime`: `pub use
// runtime::{CaptureBackend, RenderConfig, RenderGpu, RenderHandles};`.
pub use self::capture::CaptureBackend;
pub use self::config::RenderConfig;
pub use self::pipeline::{RenderGpu, RenderHandles};

// The moved `#[runtime] impl NativeActor for RenderCapability` body names the
// `#[runtime]` attribute, the cap kinds (the drawing kinds via the parent's
// `kinds` re-export, `CaptureFrame` / `CaptureFrameResult` from `aether_kinds`),
// and the substrate ctx types it previously reached through the parent's shared
// `any(render-runtime, runtime)` seam — now sourced here beside the body.
use aether_actor::runtime;

use aether_kinds::{CaptureFrame, CaptureFrameResult};

use aether_substrate::Manual;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::{
    Camera, CreateTexture, CreateTextureResult, DRAW_TRIANGLE_BYTES, DrawSolidQuads,
    DrawTexturedQuads, DrawTriangle, RenderCapability, SolidQuad, TexturedQuad, UpdateTexture,
};

// These seam items are `pub(in crate::render)` (visible in `render`) in their
// now-nested child modules, so the re-export up to runtime level keeps that
// exact visibility — the `use runtime::*` glob in `mod.rs` reaches them from
// `render`, the scope the co-located test module names them in. `pub use`
// would try to widen them to `pub` and fail (E0364/E0365).
pub(in crate::render) use self::capture::resolve_reference;
pub(in crate::render) use self::quad::QuadBatch;
pub(in crate::render) use self::texture::{
    StagedTexture, TextureRegistry, WHITE_TEXTURE_ID, expected_pixel_bytes,
};

/// `aether.render` runtime state (ADR-0066). Holds [`RenderHandles`] (the
/// driver-facing accumulator state plus GPU bundle) and the per-instance
/// [`RenderConfig`], plus the substrate registry + mailer captured at init
/// for the `capture_frame` resolve-bundle / push-pre-mails path. The
/// dispatcher holds this as the cap's state and routes envelopes through
/// the macro-emitted `Dispatch` impl; the addressing identity is the
/// distinct ZST [`super::RenderCapability`]. Driver glue fetches the
/// handle bundle via `DriverCtx::handle::<RenderHandles>()` (published in
/// `init`), not through this state. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
pub struct RenderCapabilityState {
    pub(super) handles: RenderHandles,
    pub(super) config: RenderConfig,
    /// Substrate registry and mailer captured at init for the
    /// `capture_frame` resolve-bundle / push-pre-mails path. Both are
    /// Arc-shared with every other cap and the chassis loop.
    pub(super) registry: Arc<Registry>,
    pub(super) mailer: Arc<Mailer>,
}

#[runtime]
impl NativeActor for RenderCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// driver-facing accumulator handles + config + substrate handles.
    type State = RenderCapabilityState;

    type Config = RenderConfig;

    /// Components mail `aether.draw_triangle` and `aether.camera`
    /// (kind ids) to this mailbox; the GPU recorder pulls from here.
    /// The `aether.<name>` form is the post-ADR-0074 Phase 5
    /// convention for chassis-owned mailboxes; ADR-0074 §Decision 7
    /// folded the camera mailbox into render under this name.
    const NAMESPACE: &'static str = "aether.render";

    /// Allocate the accumulator state up front. Idempotent on the
    /// driver-facing side: every chassis main passes a fresh
    /// `RenderConfig`; init only sets up the in-process buffers and
    /// the wgpu `OnceLock` (the driver fills it in `resumed` /
    /// post-`build_passive`).
    ///
    /// `last_submitted` mirrors `frame_vertices`'s capacity so the
    /// swap inside `record_frame` (iamacoffeepot/aether#847) lands
    /// a full-capacity buffer back into the live slot — without the
    /// pre-allocation, the first frame's swap would leave the live
    /// accumulator at `last_submitted`'s starting capacity (zero)
    /// and the next tick's `on_draw_triangle` would reallocate
    /// from scratch.
    fn init(
        config: RenderConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<RenderCapabilityState, BootError> {
        let handles = RenderHandles {
            frame_vertices: Arc::new(Mutex::new(Vec::<u8>::with_capacity(
                config.vertex_buffer_bytes,
            ))),
            last_submitted: Arc::new(Mutex::new(Vec::<u8>::with_capacity(
                config.vertex_buffer_bytes,
            ))),
            triangles_rendered: Arc::new(AtomicU64::new(0)),
            camera_state: Arc::new(Mutex::new(IDENTITY_VIEW_PROJ)),
            quad_frame: Arc::new(Mutex::new(Vec::new())),
            quad_last_submitted: Arc::new(Mutex::new(Vec::new())),
            textures: Arc::new(Mutex::new(TextureRegistry::new())),
            gpu: Arc::new(OnceLock::new()),
        };
        let mailer = ctx.mailer();
        let registry = Arc::clone(mailer.registry());
        // Issue 629 / Phase A: publish the driver-facing handle
        // bundle on the chassis's `ExportedHandles` map so the
        // desktop driver retrieves it via `DriverCtx::handle::<RenderHandles>()`.
        ctx.publish_handle(handles.clone());
        Ok(RenderCapabilityState {
            handles,
            config,
            registry,
            mailer,
        })
    }

    /// `DrawTriangle` handler. Slice-typed because `Mailbox::send_many`
    /// (ADR-0019) packs `count` triangles into one envelope — the
    /// macro decodes the whole payload as `&[DrawTriangle]` so a
    /// batched mesh reaches the cap intact. Truncates at the cap
    /// boundary so a single oversized mesh degrades gracefully
    /// instead of collapsing the whole frame downstream; rounds to
    /// whole triangles so the GPU vertex buffer never sees a half-
    /// triangle.
    ///
    /// # Agent
    /// Components mail one or more `DrawTriangle`s (cast-shape,
    /// `DRAW_TRIANGLE_BYTES` per triangle, batched via `send_many`)
    /// per tick. Fire-and-forget; the cap accumulates into
    /// `frame_vertices` until the chassis driver records the frame.
    #[handler]
    fn on_draw_triangle(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mails: &[DrawTriangle]) {
        if let Some(obs) = &state.config.observed_kinds {
            obs.lock()
                .expect("mutex poisoned; fail-fast per ADR-0063")
                .push(<DrawTriangle as Kind>::NAME.into());
        }
        let bytes: &[u8] = bytemuck::cast_slice(mails);
        let cap_bytes = state.config.vertex_buffer_bytes;
        let mut verts = state
            .handles
            .frame_vertices
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        let available = cap_bytes.saturating_sub(verts.len());
        let write_len = bytes.len().min(available);
        let write_len = write_len - (write_len % DRAW_TRIANGLE_BYTES);
        if write_len > 0 {
            verts.extend_from_slice(&bytes[..write_len]);
            state
                .handles
                .triangles_rendered
                .fetch_add((write_len / DRAW_TRIANGLE_BYTES) as u64, Ordering::Relaxed);
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

    /// `Camera` handler. Latest-value-wins semantics: each successful
    /// mail overwrites; the prior value is replaced wholesale.
    /// Initialised in `init` to [`IDENTITY_VIEW_PROJ`] so the first
    /// frame draws unchanged until a camera component starts
    /// publishing.
    ///
    /// # Agent
    /// Camera components mail `aether.camera { view_proj: [f32; 16] }`
    /// to this mailbox. Fire-and-forget; latest value wins.
    #[handler]
    fn on_camera(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Camera) {
        if let Some(obs) = &state.config.observed_kinds {
            obs.lock()
                .expect("mutex poisoned; fail-fast per ADR-0063")
                .push(<Camera as Kind>::NAME.into());
        }
        *state
            .handles
            .camera_state
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063") = mail.view_proj;
    }

    /// `CaptureFrame` handler. The cap dispatcher thread doesn't
    /// own the wgpu `Device` — that lives on the chassis main
    /// thread — so capture is a two-phase handoff: this handler
    /// resolves the request and parks it on `CaptureBackend.queue`,
    /// the chassis main loop takes from there on the next redraw,
    /// performs the GPU readback, dispatches the `after_mails`
    /// bundle, and replies to the original sender.
    ///
    /// Abort-on-first-failure: if either bundle has an unknown
    /// kind / mailbox the whole request errors before any pre-mail
    /// is pushed. Decode failure, queue full, and a dead wake
    /// target also reply inline; only the readback path replies
    /// from the render thread.
    ///
    /// # Agent
    /// Mail `aether.render.capture_frame { mails, after_mails }`
    /// for an atomic "set X, capture, restore X" call. Reply is
    /// `aether.render.capture_frame_result` carrying the PNG on
    /// success or a free-form reason on failure.
    #[handler::manual]
    fn on_capture_frame(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: CaptureFrame,
    ) {
        if let Some(obs) = &state.config.observed_kinds {
            obs.lock()
                .expect("mutex poisoned; fail-fast per ADR-0063")
                .push(<CaptureFrame as Kind>::NAME.into());
        }

        let sender = ctx.reply_target();
        let Some(backend) = state.config.capture_backend.as_ref() else {
            tracing::warn!(
                target: "aether_capabilities::render",
                "RenderCapability received capture_frame without capture_backend; replying Err",
            );
            return;
        };

        let pre = match resolve_bundle(&state.registry, &mail.mails, "capture bundle") {
            Ok(v) => v,
            Err(error) => {
                backend
                    .outbound
                    .send_reply(sender, &CaptureFrameResult::Err { error });
                return;
            }
        };
        let after = match resolve_bundle(&state.registry, &mail.after_mails, "capture after bundle")
        {
            Ok(v) => v,
            Err(error) => {
                backend
                    .outbound
                    .send_reply(sender, &CaptureFrameResult::Err { error });
                return;
            }
        };

        // iamacoffeepot/aether#860: dispatch each pre-mail as a
        // fresh chassis-rooted chain via `send_envelope_as_root`
        // and subscribe to its settlement, so the driver can wait
        // for the full causal chain (component handler → emitted
        // DrawTriangle → render cap accumulator) to land before
        // `render_and_capture` runs. Without this gate the cross-
        // thread chain races the wake-and-render path and an
        // empty `frame_vertices` falls back to the (empty) cache
        // → solid-background frame. Same primitive RpcServer uses
        // for wire-borne Calls (a pre-mail is causally external
        // from the cap's perspective — triggered by a wire-borne
        // CaptureFrame, not forwarded from in-flight context).
        //
        // If the chassis didn't install a settlement registry
        // (some test fixtures), the loop still dispatches the
        // mails but `pre_settlements` stays empty so the driver
        // renders immediately — preserving the pre-fix behaviour
        // on those fixtures.
        let settlement_registry = state.mailer.settlement_registry().cloned();
        let mut pre_settlements = Vec::with_capacity(pre.len());
        for envelope in pre {
            let mail_id = ctx.send_envelope_as_root(
                envelope.recipient,
                envelope.kind,
                envelope.payload.bytes(),
            );
            if let Some(reg) = settlement_registry.as_deref() {
                pre_settlements.push(reg.subscribe_settlement(mail_id));
            }
        }

        // ADR-0106 / iamacoffeepot/aether#1758: capture is a deferred
        // reply — the render thread sends the reply a frame later, off
        // this handler's dispatch window. Retain the dispatched
        // `CaptureFrame` envelope as an `InboundMail` guard via
        // `take_inbound`: the dispatcher's settlement tail then sees
        // `None` and does not discharge, so the guard's un-fired
        // `record_finished` keeps the inbound's chain open until the
        // render thread replies through `reply.reply(&result)` and
        // drops it (recording the reply's `Sent` before the inbound's
        // `Finished`, ADR-0080 §6). This retires the hand-rolled
        // `SettlementHold` + reply-id mint (#1273 / #1719). The
        // early-return branches above reply synchronously inside this
        // handler's own dispatch window, so they leave the inbound for
        // the dispatcher's tail to settle and don't take the guard.

        // iamacoffeepot/aether#1780: read the reference PNG synchronously
        // on the cap dispatcher thread (not the render thread) so all
        // filesystem I/O stays off the render hot path. The render thread
        // only runs the CPU-side MAE comparison against the pre-fetched
        // bytes.
        let reference =
            match resolve_reference(state.config.assets_dir.as_deref(), mail.similarity.as_ref()) {
                Ok(reference) => reference,
                Err(error) => {
                    backend
                        .outbound
                        .send_reply(sender, &CaptureFrameResult::Err { error });
                    return;
                }
            };

        let inbound = ctx.take_inbound();
        let pending = PendingCapture {
            reply: inbound,
            after_mails: after,
            pre_settlements,
            checks: mail.checks,
            reference,
        };
        // A rejected request (`Err`) is handed back so its retained
        // guard can carry the synchronous `Err` reply before it drops
        // — settling after the reply, ADR-0080 §6.
        if let Err(rejected) = backend.queue.request(pending) {
            rejected.reply.reply(&CaptureFrameResult::Err {
                error: "capture already pending; try again once the in-flight \
                        request completes"
                    .to_owned(),
            });
            return;
        }

        if let Err(reason) = (backend.wake)()
            && let Some(rejected) = backend.queue.take()
        {
            // The wake never reached the render thread, so the request
            // is still parked: take it back and reply `Err` through its
            // retained guard, which then drops and settles the chain.
            rejected.reply.reply(&CaptureFrameResult::Err {
                error: reason.to_owned(),
            });
        }
    }

    /// `CreateTexture` handler (ADR-0105). Validates the dimensions
    /// and pixel length, stages the RGBA8 pixels CPU-side under the
    /// next session-scoped `texture_id`, and replies immediately —
    /// the wgpu texture is realized lazily at the next frame record
    /// (the GPU bundle isn't installed until the chassis driver
    /// boots). A zero dimension or a `pixels` length that doesn't
    /// match `width * height * 4` replies `Err` without registering.
    ///
    /// # Agent
    /// Mail `aether.render.create_texture { width, height, pixels }`;
    /// the reply `aether.render.create_texture_result` carries the
    /// `texture_id` to thread into `draw_textured_quads`.
    #[handler]
    fn on_create_texture(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CreateTexture,
    ) -> CreateTextureResult {
        if let Some(obs) = &state.config.observed_kinds {
            obs.lock()
                .expect("mutex poisoned; fail-fast per ADR-0063")
                .push(<CreateTexture as Kind>::NAME.into());
        }
        let expected = expected_pixel_bytes(mail.width, mail.height);
        let Some(expected) = expected else {
            return CreateTextureResult::Err {
                error: format!(
                    "texture dimensions {}x{} overflow or are zero",
                    mail.width, mail.height
                ),
            };
        };
        if mail.pixels.len() != expected {
            return CreateTextureResult::Err {
                error: format!(
                    "pixels length {} does not match width*height*4 = {expected}",
                    mail.pixels.len()
                ),
            };
        }
        let mut registry = state
            .handles
            .textures
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        let texture_id = registry.next_id;
        registry.next_id += 1;
        registry.entries.insert(
            texture_id,
            StagedTexture {
                width: mail.width,
                height: mail.height,
                pixels: mail.pixels,
                realized: None,
                dirty: true,
            },
        );
        drop(registry);
        CreateTextureResult::Ok { texture_id }
    }

    /// `UpdateTexture` handler (ADR-0105). Overwrites a sub-rectangle
    /// of a staged texture's pixels and dirties it so the next record
    /// re-uploads. Fire-and-forget: an unknown `texture_id`, an
    /// out-of-bounds rect, or a `pixels` length that doesn't match the
    /// sub-rect logs and drops without touching the staging buffer.
    ///
    /// # Agent
    /// Mail `aether.render.update_texture { texture_id, x, y, width,
    /// height, pixels }` to grow an atlas; no reply.
    #[handler]
    fn on_update_texture(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UpdateTexture) {
        if let Some(obs) = &state.config.observed_kinds {
            obs.lock()
                .expect("mutex poisoned; fail-fast per ADR-0063")
                .push(<UpdateTexture as Kind>::NAME.into());
        }
        let mut registry = state
            .handles
            .textures
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        let Some(entry) = registry.entries.get_mut(&mail.texture_id) else {
            tracing::warn!(
                target: "aether_capabilities::render",
                texture_id = mail.texture_id,
                "update_texture for unknown texture id; dropping",
            );
            return;
        };
        if !entry.apply_subrect(mail.x, mail.y, mail.width, mail.height, &mail.pixels) {
            tracing::warn!(
                target: "aether_capabilities::render",
                texture_id = mail.texture_id,
                "update_texture rect out of bounds, zero-sized, or pixel length mismatch; \
                 dropping",
            );
        }
    }

    /// `DrawTexturedQuads` handler (ADR-0105). Accumulates the batch
    /// into the per-frame quad accumulator with the same immediate-
    /// mode contract as `on_draw_triangle`: the driver consumes it at
    /// record time. An unknown `texture_id` or a `World` space is
    /// warn-dropped at encode, not here, so the accumulate path stays
    /// a cheap push.
    ///
    /// # Agent
    /// Mail `aether.render.draw_textured_quads { texture_id, space,
    /// quads }` every frame the quads should appear; no reply.
    #[handler]
    fn on_draw_textured_quads(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: DrawTexturedQuads,
    ) {
        if let Some(obs) = &state.config.observed_kinds {
            obs.lock()
                .expect("mutex poisoned; fail-fast per ADR-0063")
                .push(<DrawTexturedQuads as Kind>::NAME.into());
        }
        state
            .handles
            .quad_frame
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063")
            .push(QuadBatch {
                texture_id: mail.texture_id,
                space: mail.space,
                quads: mail.quads,
            });
    }

    /// `DrawSolidQuads` handler (ADR-0107 §4). Expands each `SolidQuad`
    /// into a `TexturedQuad` covering the full uv of a reserved internal
    /// 1×1 white texture, tinted by `color` — so the white texel ×
    /// tint produces the flat fill color with no new GPU pipeline.
    /// Lazily inserts the white texture into the registry on first call.
    /// Accumulates into the same `quad_frame` accumulator as
    /// `on_draw_textured_quads`; immediate-mode contract is identical.
    ///
    /// # Agent
    /// Mail `aether.render.draw_solid_quads { space, quads }` every
    /// frame the rects should appear; no reply.
    #[handler]
    fn on_draw_solid_quads(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: DrawSolidQuads,
    ) {
        if let Some(obs) = &state.config.observed_kinds {
            obs.lock()
                .expect("mutex poisoned; fail-fast per ADR-0063")
                .push(<DrawSolidQuads as Kind>::NAME.into());
        }
        let mut registry = state
            .handles
            .textures
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063");
        registry
            .entries
            .entry(WHITE_TEXTURE_ID)
            .or_insert_with(|| StagedTexture {
                width: 1,
                height: 1,
                pixels: vec![255, 255, 255, 255],
                realized: None,
                dirty: true,
            });
        drop(registry);
        let quads: Vec<TexturedQuad> = mail
            .quads
            .into_iter()
            .map(
                |SolidQuad {
                     x,
                     y,
                     width,
                     height,
                     color,
                 }| TexturedQuad {
                    x,
                    y,
                    width,
                    height,
                    u0: 0.0,
                    v0: 0.0,
                    u1: 1.0,
                    v1: 1.0,
                    tint: color,
                },
            )
            .collect();
        state
            .handles
            .quad_frame
            .lock()
            .expect("mutex poisoned; fail-fast per ADR-0063")
            .push(QuadBatch {
                texture_id: WHITE_TEXTURE_ID,
                space: mail.space,
                quads,
            });
    }
}
