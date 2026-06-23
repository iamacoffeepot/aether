//! `aether.render` cap. Owns the render mailbox surface plus the
//! driver-facing accumulator state ([`RenderHandles`]) and GPU bundle
//! ([`RenderGpu`]). Post-ADR-0082 the chassis gates frame submit on
//! settlement of the `LifecycleAdvance` chain root — render's
//! `DrawTriangle` / `aether.camera` mail are descendants of that root,
//! so they're integrated before submit without a per-mailbox drain
//! counter.
//!
//! Driver-side state (wgpu device, queue, pipeline, offscreen
//! targets, accumulator buffers) lives on [`RenderHandles`] in the
//! `pipeline` submodule. The driver fetches the booted cap via
//! `DriverCtx::actor::<RenderCapability>()` and clones `.handles()`
//! from there. Phase 4 keeps the GPU lifecycle, encoder creation, and
//! presentation in the chassis driver — this capability owns only the
//! mail surface and accumulator state.
//!
//! The cap's drawing + texture mail kinds live in [`kinds`] (ADR-0121):
//! they ride the always-on (marker-only `render`) region so a wasm
//! guest sees the kind types for typed addressing without the
//! `render-native` GPU stack. The capture-request and `FrameCheck`
//! verification kinds stay in `aether-kinds` (consumed upstream by
//! `aether-mcp` and the substrate core), as do the `QuadSpace` /
//! `QuadScale` projection types the `aether.text` kinds share.
//!
//! The decomposition is along the cap's cohesion seams: `pipeline`
//! (GPU bundle + accumulator handles), `texture` (the texture
//! registry), `quad` (the quad-batch accumulator), and `capture`
//! (the cross-thread readback machinery).
//!
//! [`HeadlessRenderCapability`] is the chassis-without-GPU companion:
//! same `aether.render` mailbox, no-op `DrawTriangle` / `Camera`
//! handlers (so desktop-designed components don't warn-storm),
//! `Err`-replying `CaptureFrame` handler. Headless chassis composes it
//! in place of [`RenderCapability`] (issue 603 Phase 2 § Resolved
//! Decision 5).

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]
// Frame-vertex / last-submitted Mutex guards are held through the
// per-frame swap and append sequence on purpose — the swap and
// subsequent length math read the buffer's current state and write
// back; releasing the guard mid-sequence opens a TOCTOU window
// where a sibling tick's producer mutates the buffer in between.
#![allow(clippy::significant_drop_tightening)]

// The cap's drawing + texture mail kinds (ADR-0121). Always-on (the
// `render` marker feature gates the whole module) so a wasm guest on the
// marker-only `render` feature sees the kind types.
pub mod kinds;
pub use kinds::*;

// Native impl seams. Gated identically to the `#[bridge]`-emitted
// `mod native` body (`not(wasm) AND render-native`) so they're present
// exactly when that body references them.
#[cfg(all(not(target_family = "wasm"), feature = "render-native"))]
mod capture;
#[cfg(all(not(target_family = "wasm"), feature = "render-native"))]
mod pipeline;
#[cfg(all(not(target_family = "wasm"), feature = "render-native"))]
mod quad;
#[cfg(all(not(target_family = "wasm"), feature = "render-native"))]
mod texture;

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate). The drawing kinds come
// from the local `kinds` module (via the glob re-export above);
// `CaptureFrame` stays in `aether-kinds` (consumed by `aether-mcp`).
use aether_kinds::CaptureFrame;

// Auxiliary native-only types the chassis driver consumes alongside
// `RenderCapability`. `#[bridge]` only re-exports the actor type
// itself; these need explicit re-exports. Keyed on the `render-native`
// feature so wasm components that opt into the marker-only `render`
// feature see only the cap stub + Actor / HandlesKind impls, not
// these heavy GPU-bound types.
#[cfg(all(not(target_family = "wasm"), feature = "render-native"))]
pub use self::capture::CaptureBackend;
#[cfg(all(not(target_family = "wasm"), feature = "render-native"))]
pub use self::native::RenderConfig;
#[cfg(all(not(target_family = "wasm"), feature = "render-native"))]
pub use self::pipeline::{RenderGpu, RenderHandles};

// `HeadlessRenderCapability` is exported through `#[bridge]`'s
// auto-emitted re-export. It carries no auxiliary native-only types,
// so nothing extra to surface here.

#[aether_actor::bridge(singleton, feature = "render-native")]
mod native {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    use aether_actor::actor;
    use aether_data::Kind;
    use aether_kinds::CaptureFrameResult;
    use aether_substrate::Manual;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::capture::PendingCapture;
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::helpers::resolve_bundle;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::render::{IDENTITY_VIEW_PROJ, VERTEX_BUFFER_BYTES};

    use super::capture::{CaptureBackend, resolve_reference};
    use super::pipeline::RenderHandles;
    use super::quad::QuadBatch;
    use super::texture::{StagedTexture, TextureRegistry, WHITE_TEXTURE_ID, expected_pixel_bytes};
    use super::{
        Camera, CaptureFrame, CreateTexture, CreateTextureResult, DRAW_TRIANGLE_BYTES,
        DrawSolidQuads, DrawTexturedQuads, DrawTriangle, SolidQuad, TexturedQuad, UpdateTexture,
    };

    /// Configuration for [`RenderCapability`]. `vertex_buffer_bytes` is
    /// the maximum bytes the render accumulator will hold before
    /// truncating with a warn — desktop and test-bench both pass
    /// [`aether_substrate::render::VERTEX_BUFFER_BYTES`].
    ///
    /// `observed_kinds`, when set, has every successfully-dispatched
    /// inbound mail's kind name pushed to it from the cap's `#[handler]`
    /// methods — used by the in-process test-bench to assert what kinds
    /// the cap has seen. Production chassis leave it `None` (zero
    /// overhead). Decode failures and unknown kinds don't push (the
    /// macro miss path warn-logs at the chassis-side dispatcher and
    /// short-circuits before any handler runs); pre-PR-E2 the legacy
    /// path pushed the raw `kind_name` regardless of dispatch outcome,
    /// but tests only use the list as a diagnostic in failure messages
    /// so the narrower semantic is fine.
    #[derive(Clone)]
    pub struct RenderConfig {
        pub vertex_buffer_bytes: usize,
        pub observed_kinds: Option<Arc<Mutex<Vec<String>>>>,
        /// Driver-side capture backend. Desktop and test-bench populate
        /// it with their `CaptureQueue` + chassis-loop wake hook;
        /// chassis without a render thread (the in-crate tests below)
        /// leave it `None` and `aether.render.capture_frame` mail
        /// replies `Err`. Headless declines capture by composing a
        /// distinct `HeadlessRenderCapability` instead, so this `None`
        /// branch is exercised only in the test fixtures here.
        pub capture_backend: Option<CaptureBackend>,
        /// Resolved path for the `"assets"` namespace, used by the
        /// `capture_frame` handler to read reference images for
        /// similarity checks (iamacoffeepot/aether#1780). The handler
        /// reads the reference PNG synchronously (on the cap dispatcher
        /// thread, not the render thread) and passes the raw bytes
        /// through `PendingCapture.reference`. `None` disables
        /// similarity checks with a descriptive `Err` reply.
        pub assets_dir: Option<PathBuf>,
    }

    impl Default for RenderConfig {
        fn default() -> Self {
            Self {
                vertex_buffer_bytes: VERTEX_BUFFER_BYTES,
                observed_kinds: None,
                capture_backend: None,
                assets_dir: None,
            }
        }
    }

    /// `aether.render` mailbox cap. Holds [`RenderHandles`] (the
    /// driver-facing accumulator state plus GPU bundle) and the
    /// per-instance config. The dispatcher thread holds an
    /// `Arc<Self>` and routes `aether.draw_triangle` / `aether.camera`
    /// mail through the macro-emitted `NativeDispatch` impl. Driver
    /// glue fetches handles via
    /// `DriverCtx::actor::<RenderCapability>()` (post-init) and clones
    /// the cheap Arc-shared bundle.
    pub struct RenderCapability {
        handles: RenderHandles,
        config: RenderConfig,
        /// Substrate registry and mailer captured at init for the
        /// `capture_frame` resolve-bundle / push-pre-mails path. Both
        /// are Arc-shared with every other cap and the chassis loop.
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
    }

    impl RenderCapability {
        /// Cheap clone of the driver-facing handles bundle. Call this on
        /// the booted `Arc<RenderCapability>` (fetched via
        /// `DriverCtx::actor`) — every field is Arc-shared, so the clone
        /// is just refcount bumps.
        #[must_use]
        pub fn handles(&self) -> RenderHandles {
            self.handles.clone()
        }
    }

    #[actor]
    impl NativeActor for RenderCapability {
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
        fn init(config: RenderConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
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
            Ok(Self {
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
        fn on_draw_triangle(&self, _ctx: &mut NativeCtx<'_>, mails: &[DrawTriangle]) {
            if let Some(obs) = &self.config.observed_kinds {
                obs.lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063")
                    .push(<DrawTriangle as Kind>::NAME.into());
            }
            let bytes: &[u8] = bytemuck::cast_slice(mails);
            let cap_bytes = self.config.vertex_buffer_bytes;
            let mut verts = self
                .handles
                .frame_vertices
                .lock()
                .expect("mutex poisoned; fail-fast per ADR-0063");
            let available = cap_bytes.saturating_sub(verts.len());
            let write_len = bytes.len().min(available);
            let write_len = write_len - (write_len % DRAW_TRIANGLE_BYTES);
            if write_len > 0 {
                verts.extend_from_slice(&bytes[..write_len]);
                self.handles
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
        fn on_camera(&self, _ctx: &mut NativeCtx<'_>, mail: Camera) {
            if let Some(obs) = &self.config.observed_kinds {
                obs.lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063")
                    .push(<Camera as Kind>::NAME.into());
            }
            *self
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
        fn on_capture_frame(&self, ctx: &mut NativeCtx<'_, Manual>, mail: CaptureFrame) {
            if let Some(obs) = &self.config.observed_kinds {
                obs.lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063")
                    .push(<CaptureFrame as Kind>::NAME.into());
            }

            let sender = ctx.reply_target();
            let Some(backend) = self.config.capture_backend.as_ref() else {
                tracing::warn!(
                    target: "aether_capabilities::render",
                    "RenderCapability received capture_frame without capture_backend; replying Err",
                );
                return;
            };

            let pre = match resolve_bundle(&self.registry, &mail.mails, "capture bundle") {
                Ok(v) => v,
                Err(error) => {
                    backend
                        .outbound
                        .send_reply(sender, &CaptureFrameResult::Err { error });
                    return;
                }
            };
            let after =
                match resolve_bundle(&self.registry, &mail.after_mails, "capture after bundle") {
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
            let settlement_registry = self.mailer.settlement_registry().cloned();
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
            let reference = match resolve_reference(
                self.config.assets_dir.as_deref(),
                mail.similarity.as_ref(),
            ) {
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
            &self,
            _ctx: &mut NativeCtx<'_>,
            mail: CreateTexture,
        ) -> CreateTextureResult {
            if let Some(obs) = &self.config.observed_kinds {
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
            let mut registry = self
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
        fn on_update_texture(&self, _ctx: &mut NativeCtx<'_>, mail: UpdateTexture) {
            if let Some(obs) = &self.config.observed_kinds {
                obs.lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063")
                    .push(<UpdateTexture as Kind>::NAME.into());
            }
            let mut registry = self
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
        fn on_draw_textured_quads(&self, _ctx: &mut NativeCtx<'_>, mail: DrawTexturedQuads) {
            if let Some(obs) = &self.config.observed_kinds {
                obs.lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063")
                    .push(<DrawTexturedQuads as Kind>::NAME.into());
            }
            self.handles
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
        fn on_draw_solid_quads(&self, _ctx: &mut NativeCtx<'_>, mail: DrawSolidQuads) {
            if let Some(obs) = &self.config.observed_kinds {
                obs.lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063")
                    .push(<DrawSolidQuads as Kind>::NAME.into());
            }
            let mut registry = self
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
            self.handles
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

    #[cfg(test)]
    mod tests {
        use std::time::Duration;

        use super::*;
        use crate::test_chassis::TestChassis;
        use aether_actor::Addressable;
        use aether_kinds::QuadSpace;
        use aether_kinds::trace::Nanos;
        use aether_substrate::chassis::builder::{Builder, PassiveChassis};
        use aether_substrate::mail::MailId;
        use aether_substrate::mail::MailRef;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::registry::OwnedDispatch;
        use aether_substrate::mail::registry::{MailboxEntry, Registry};
        use aether_substrate::mail::{KindId, Source};
        use std::thread;

        use crate::test_chassis::fresh_substrate;

        /// ADR-0099 §3/§5: a real `#[bridge(singleton)]` chassis cap keeps
        /// the default [`aether_actor::Singleton::resolve`], so its id is the
        /// depth-1 fixed point — exactly the `mailbox_id_from_name(NAMESPACE)`
        /// value it had before the lineage fold, regardless of the caller's
        /// carry. Guards the frozen-vocabulary claim: #1431 must not move any
        /// root-cap id.
        // Asserts the cap's resolved id against the frozen depth-1 name hash —
        // the primitive is the reference value under test.
        #[allow(clippy::disallowed_methods)]
        #[test]
        fn render_capability_resolves_to_frozen_depth_one_id() {
            use aether_data::mailbox_id_from_name;

            let frozen = mailbox_id_from_name(<RenderCapability as Addressable>::NAMESPACE);
            assert_eq!(<RenderCapability as Addressable>::resolve(0, ()), frozen);
            assert_eq!(
                <RenderCapability as Addressable>::resolve(0xFFFF_FFFF_FFFF_FFFF, ()),
                frozen,
            );
        }

        /// Boots a passive `TestChassis` with a default `RenderCapability`.
        /// Collapses the four-line `Builder::<TestChassis>::new(...)` chain
        /// every render test repeated (issue 795).
        fn build_render_chassis(
            registry: &Arc<Registry>,
            mailer: &Arc<Mailer>,
        ) -> PassiveChassis<TestChassis> {
            Builder::<TestChassis>::new(Arc::clone(registry), Arc::clone(mailer))
                .with_actor::<RenderCapability>(RenderConfig::default())
                .build_passive()
                .expect("build succeeds")
        }

        fn deliver(registry: &Registry, name: &str, kind: KindId, payload: &[u8]) {
            let id = registry.lookup(name).expect("mailbox registered");
            let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry exists")
            else {
                panic!("expected mailbox entry for {name}");
            };
            handler.enqueue(OwnedDispatch::disarmed(
                kind,
                "test.kind".to_owned(),
                None,
                Source::NONE,
                MailRef::from(payload.to_vec()),
                1,
                MailId::NONE,
                MailId::NONE,
                None,
                Nanos(0),
                0,
                aether_data::MailboxId(0),
            ));
        }

        #[test]
        fn capability_claims_render_mailbox_only() {
            let (registry, mailer) = fresh_substrate();
            let chassis = build_render_chassis(&registry, &mailer);
            assert_eq!(chassis.len(), 1);
            assert!(registry.lookup(RenderCapability::NAMESPACE).is_some());
            // Camera mailbox retired (ADR-0074 §Decision 7).
            assert!(registry.lookup("aether.sink.camera").is_none());
        }

        // ADR-0082 retired the frame-bound pending counter; the
        // DrawTriangle → render dispatch path is now covered end-to-end
        // by the bundle scenario tests (`tick_roundtrip_component_to_sink`
        // and the `test_bench_scenario` suite), which exercise it through
        // real settlement rather than a per-mailbox counter poll.

        /// ADR-0107 §4: `draw_solid_quads` accumulates into `quad_frame` under
        /// the reserved `WHITE_TEXTURE_ID` and records its kind name in
        /// `observed_kinds`. Verifies the expand-to-TexturedQuad path and the
        /// lazy white-texture insertion without a GPU.
        #[test]
        fn draw_solid_quads_accumulates_and_observed() {
            let observed = Arc::new(Mutex::new(Vec::<String>::new()));
            let config = RenderConfig {
                observed_kinds: Some(Arc::clone(&observed)),
                ..RenderConfig::default()
            };
            let (registry, mailer) = fresh_substrate();
            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<RenderCapability>(config)
                .build_passive()
                .expect("build succeeds");
            let handles = chassis
                .handle::<RenderHandles>()
                .expect("RenderCapability publishes RenderHandles");

            let mail = DrawSolidQuads {
                space: QuadSpace::Screen,
                quads: vec![SolidQuad {
                    x: 10.0,
                    y: 20.0,
                    width: 30.0,
                    height: 40.0,
                    color: [1.0, 0.0, 0.5, 0.8],
                }],
            };
            let payload = mail.encode_into_bytes();
            deliver(
                &registry,
                RenderCapability::NAMESPACE,
                <DrawSolidQuads as Kind>::ID,
                &payload,
            );

            thread::sleep(Duration::from_millis(50));

            let seen = observed
                .lock()
                .expect("observed_kinds mutex is not poisoned")
                .clone();
            assert!(
                seen.contains(&DrawSolidQuads::NAME.to_owned()),
                "draw_solid_quads handler should push its kind name; observed: {seen:?}",
            );

            let batches = handles
                .quad_frame
                .lock()
                .expect("quad_frame mutex is not poisoned")
                .clone();
            assert_eq!(
                batches.len(),
                1,
                "one QuadBatch should be in the accumulator"
            );
            assert_eq!(
                batches[0].texture_id, WHITE_TEXTURE_ID,
                "batch must use the reserved white texture id",
            );
            assert_eq!(
                batches[0].quads.len(),
                1,
                "batch must contain the one expanded quad"
            );
            assert_eq!(
                batches[0].quads[0].tint,
                [1.0, 0.0, 0.5, 0.8],
                "expanded quad tint must match the SolidQuad color",
            );
            assert_eq!(batches[0].quads[0].width, 30.0);

            let tex_present = handles
                .textures
                .lock()
                .expect("textures mutex is not poisoned")
                .entries
                .contains_key(&WHITE_TEXTURE_ID);
            assert!(
                tex_present,
                "white texture must be lazily inserted on first send"
            );

            drop(chassis);
        }

        #[test]
        fn camera_kind_drops_wrong_length_payload() {
            let (registry, mailer) = fresh_substrate();
            let chassis = build_render_chassis(&registry, &mailer);

            // 16 bytes — wrong length, decode fails, macro miss path
            // logs warn at chassis-side dispatcher; identity unchanged.
            deliver(
                &registry,
                RenderCapability::NAMESPACE,
                <Camera as Kind>::ID,
                &[1u8; 16],
            );

            thread::sleep(Duration::from_millis(50));
            // No further assertion on internal state — passive chassis
            // doesn't expose `Arc<RenderCapability>`. Decode failure is
            // observable via the macro miss path's warn-log; this test
            // asserts shutdown still proceeds cleanly (no dispatcher
            // panic on malformed input).
            drop(chassis);
        }
    }
}

#[aether_actor::bridge(singleton)]
mod native_headless {
    use std::sync::Arc;

    use aether_actor::actor;
    use aether_kinds::CaptureFrameResult;
    use aether_substrate::Manual;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::outbound::HubOutbound;

    use super::{
        Camera, CaptureFrame, CreateTexture, CreateTextureResult, DrawSolidQuads,
        DrawTexturedQuads, DrawTriangle, UpdateTexture,
    };
    use std::io;

    /// Chassis-without-GPU companion to [`super::RenderCapability`].
    /// Claims the same `aether.render` mailbox so desktop-designed
    /// components loaded on headless can mail `DrawTriangle` /
    /// `aether.camera` / `aether.render.capture_frame` against a known
    /// recipient — `DrawTriangle` and `Camera` no-op (the warn-storm
    /// sink-replacement role pre-issue-603 Phase 2), `CaptureFrame`
    /// replies `Err` so MCP `capture_frame` fails fast instead of
    /// timing out.
    ///
    /// Headless chassis composes one of [`Self`] / [`super::RenderCapability`],
    /// never both — the chassis builder rejects double-claiming a
    /// mailbox.
    pub struct HeadlessRenderCapability {
        outbound: Arc<HubOutbound>,
    }

    #[actor]
    impl NativeActor for HeadlessRenderCapability {
        type Config = ();

        const NAMESPACE: &'static str = "aether.render";

        fn init(_config: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
                BootError::Other(Box::new(io::Error::other(
                    "HubOutbound must be wired on Mailer before \
                         HeadlessRenderCapability::init (chassis main connects the hub before \
                         the Builder chain)",
                )))
            })?;
            Ok(Self { outbound })
        }

        /// `DrawTriangle` lands here as a no-op so headless boots of
        /// desktop-designed components (which emit `DrawTriangle` every
        /// tick) don't trip the unknown-mailbox warn path.
        // `&self` keeps the dispatch ABI (ADR-0033 / ADR-0038); the body
        // is a no-op by design — see the doc comment above.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_draw_triangle(&self, _ctx: &mut NativeCtx<'_>, _mails: &[DrawTriangle]) {}

        /// `Camera` lands here as a no-op for the same reason as
        /// `on_draw_triangle` — desktop-designed components publish
        /// `aether.camera` every tick.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_camera(&self, _ctx: &mut NativeCtx<'_>, _mail: Camera) {}

        /// `CaptureFrame` replies `Err` inline so MCP `capture_frame`
        /// fails fast on headless instead of hanging on a reply that
        /// never comes. Mirrors ADR-0035 §Consequences fail-fast shape
        /// for `set_window_mode`.
        #[handler::manual]
        fn on_capture_frame(&self, ctx: &mut NativeCtx<'_, Manual>, _mail: CaptureFrame) {
            self.outbound.send_reply(
                ctx.reply_target(),
                &CaptureFrameResult::Err {
                    error: "unsupported on headless chassis — no GPU".to_owned(),
                },
            );
        }

        /// `CreateTexture` replies `Err` inline so an agent that creates a
        /// texture against a headless chassis fails fast instead of
        /// waiting on a reply that never comes — same fail-fast shape as
        /// `on_capture_frame` (ADR-0105).
        #[handler::manual]
        fn on_create_texture(&self, ctx: &mut NativeCtx<'_, Manual>, _mail: CreateTexture) {
            self.outbound.send_reply(
                ctx.reply_target(),
                &CreateTextureResult::Err {
                    error: "unsupported on headless chassis — no GPU".to_owned(),
                },
            );
        }

        /// `UpdateTexture` lands here as a no-op so desktop-designed
        /// components running on headless don't trip the unknown-mailbox
        /// warn path — mirrors `on_draw_triangle`.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_update_texture(&self, _ctx: &mut NativeCtx<'_>, _mail: UpdateTexture) {}

        /// `DrawTexturedQuads` lands here as a no-op for the same reason
        /// as `on_update_texture`.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_draw_textured_quads(&self, _ctx: &mut NativeCtx<'_>, _mail: DrawTexturedQuads) {}

        /// `DrawSolidQuads` lands here as a no-op for the same reason
        /// as `on_draw_textured_quads`.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_draw_solid_quads(&self, _ctx: &mut NativeCtx<'_>, _mail: DrawSolidQuads) {}
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_chassis::{decode_reply, test_mailer_and_rx};
        use aether_data::{MailboxId, Source, SourceAddr};
        use aether_data::{SessionToken, Uuid};
        use aether_substrate::actor::native::NativeCtx;
        use aether_substrate::actor::native::binding::NativeBinding;

        /// ADR-0105: `create_texture` against a headless chassis replies
        /// `Err` (fail-fast, no GPU) rather than hanging on a reply that
        /// never comes — mirrors `capture_frame`'s headless shape.
        #[test]
        fn headless_create_texture_replies_err() {
            let (mailer, rx) = test_mailer_and_rx();
            let outbound = mailer
                .outbound()
                .cloned()
                .expect("test_mailer_and_rx wires a loopback outbound");
            let cap = HeadlessRenderCapability { outbound };
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = NativeCtx::new_dispatching(
                &transport,
                Source::to(SourceAddr::Session(SessionToken(Uuid::nil()))),
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            );
            cap.on_create_texture(
                &mut ctx,
                CreateTexture {
                    width: 2,
                    height: 2,
                    pixels: vec![0u8; 16],
                },
            );
            match decode_reply::<CreateTextureResult>(&rx) {
                CreateTextureResult::Err { error } => {
                    assert!(
                        error.contains("headless"),
                        "headless create_texture error should name the chassis; got {error}",
                    );
                }
                CreateTextureResult::Ok { .. } => {
                    panic!("headless create_texture must reply Err, not assign an id")
                }
            }
        }

        /// ADR-0107 §4: `draw_solid_quads` on the headless chassis is a
        /// no-op — no panic, no reply, nothing accumulated. Mirrors the
        /// `on_draw_textured_quads` no-op contract.
        #[test]
        fn headless_draw_solid_quads_is_noop() {
            use aether_kinds::QuadSpace;

            let (mailer, _rx) = test_mailer_and_rx();
            let outbound = mailer
                .outbound()
                .cloned()
                .expect("test_mailer_and_rx wires a loopback outbound");
            let cap = HeadlessRenderCapability { outbound };
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = NativeCtx::new(
                &transport,
                Source::to(SourceAddr::Session(SessionToken(Uuid::nil()))),
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            );
            cap.on_draw_solid_quads(
                &mut ctx,
                DrawSolidQuads {
                    space: QuadSpace::Screen,
                    quads: vec![],
                },
            );
            // No panic and no reply enqueued — the no-op dropped the mail cleanly.
        }
    }
}
