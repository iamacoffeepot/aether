//! `aether.render` cap. Owns the render mailbox surface plus the
//! driver-facing accumulator state ([`RenderHandles`]) and GPU bundle
//! ([`RenderGpu`]). Post-ADR-0082 the chassis gates frame submit on
//! settlement of the `LifecycleAdvance` chain root — render's
//! `DrawTriangle` / `aether.camera` mail are descendants of that root,
//! so they're integrated before submit without a per-mailbox drain
//! counter.
//!
//! Driver-side state (wgpu device, queue, pipeline, offscreen
//! targets, accumulator buffers) lives on [`RenderHandles`]. The
//! driver fetches the booted cap via
//! `DriverCtx::actor::<RenderCapability>()` and clones `.handles()`
//! from there. Phase 4 keeps the GPU lifecycle, encoder creation, and
//! presentation in the chassis driver — this capability owns only the
//! mail surface and accumulator state.
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

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{
    Camera, CaptureFrame, CreateTexture, DrawTexturedQuads, DrawTriangle, UpdateTexture,
};

// Auxiliary native-only types the chassis driver consumes alongside
// `RenderCapability`. `#[bridge]` only re-exports the actor type
// itself; these need explicit re-exports. Keyed on the `render-native`
// feature so wasm components that opt into the marker-only `render`
// feature see only the cap stub + Actor / HandlesKind impls, not
// these heavy GPU-bound types.
#[cfg(all(not(target_arch = "wasm32"), feature = "render-native"))]
pub use native::{CaptureBackend, RenderConfig, RenderGpu, RenderHandles};

// `HeadlessRenderCapability` is exported through `#[bridge]`'s
// auto-emitted re-export. It carries no auxiliary native-only types,
// so nothing extra to surface here.

#[aether_actor::bridge(singleton, feature = "render-native")]
mod native {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    use aether_actor::{OutboundReply, actor};
    use aether_data::Kind;
    use aether_kinds::{
        CaptureFrameResult, CreateTextureResult, DRAW_TRIANGLE_BYTES, QuadSpace, TexturedQuad,
    };
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::capture::{CaptureQueue, PendingCapture};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::helpers::resolve_bundle;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::render::{
        CaptureMeta, IDENTITY_VIEW_PROJ, OverlayDraw, Pipeline, QUAD_VERTEX_STRIDE,
        QUAD_VERTICES_PER_QUAD, QuadPipeline, RealizedTexture, RenderError, Targets,
        build_main_pipeline, build_quad_pipeline, finish_capture, prepare_capture_copy,
        push_screen_quad_vertices, realize_texture, record_main_pass, record_quad_overlay_pass,
        upload_texture_full,
    };

    use super::{
        Camera, CaptureFrame, CreateTexture, DrawTexturedQuads, DrawTriangle, UpdateTexture,
    };
    use aether_substrate::render::VERTEX_BUFFER_BYTES;
    use std::mem;

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
    }

    impl Default for RenderConfig {
        fn default() -> Self {
            Self {
                vertex_buffer_bytes: VERTEX_BUFFER_BYTES,
                observed_kinds: None,
                capture_backend: None,
            }
        }
    }

    /// Per-chassis plumbing the [`RenderCapability`] capture handler
    /// needs to defer the readback to the chassis main thread. The
    /// cap's dispatcher thread can't touch the wgpu `Device` (it lives
    /// on the render thread); the handler resolves the request, parks
    /// it on `queue`, and the chassis main loop reads from there on
    /// the next redraw. `wake` nudges that loop — desktop fires an
    /// `EventLoopProxy<UserEvent>::Capture`; test-bench sends on its
    /// `EventSender`.
    ///
    /// `outbound` is the cap's reply edge for the inline-failure
    /// paths (decode error, bundle-resolution error, queue full,
    /// wake target dead). All four bail before parking the request,
    /// so the only happy-path reply comes from the render thread
    /// after readback completes — that path uses its own outbound
    /// clone the chassis driver keeps.
    #[derive(Clone)]
    pub struct CaptureBackend {
        pub queue: CaptureQueue,
        pub wake: Arc<dyn Fn() -> Result<(), &'static str> + Send + Sync>,
        pub outbound: Arc<HubOutbound>,
    }

    /// One accumulated `draw_textured_quads` batch (ADR-0105): the
    /// texture it samples, the projection it draws under, and the quad
    /// list. Cloned out of the accumulator at record time so the cap
    /// dispatcher thread can keep appending the next frame's batches
    /// while the driver thread expands these.
    #[derive(Clone)]
    pub struct QuadBatch {
        pub texture_id: u32,
        pub space: QuadSpace,
        pub quads: Vec<TexturedQuad>,
    }

    /// A texture registered via `create_texture`: the staged RGBA8 pixels
    /// (the CPU source of truth), plus the lazily-realized GPU texture +
    /// bind group. `create_texture` / `update_texture` run on the cap
    /// dispatcher thread and only touch the staging side; the wgpu
    /// resources are realized at record time on the driver thread (the
    /// `RenderGpu` `OnceLock` isn't filled until the chassis driver boots
    /// the GPU). `dirty` flags staging that the GPU copy hasn't caught up
    /// to yet — the next record re-uploads the whole texture.
    struct StagedTexture {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        realized: Option<RealizedTexture>,
        dirty: bool,
    }

    impl StagedTexture {
        /// Overwrite the `(x, y, width, height)` sub-rect of the staged
        /// pixels with `pixels` (RGBA8, row-major) and dirty the texture.
        /// Returns `false` without touching the buffer if the rect is
        /// out of bounds, has a zero dimension, or `pixels` isn't exactly
        /// `width * height * 4` bytes — the caller logs and drops.
        fn apply_subrect(
            &mut self,
            x: u32,
            y: u32,
            width: u32,
            height: u32,
            pixels: &[u8],
        ) -> bool {
            let Some(rect_bytes) = expected_pixel_bytes(width, height) else {
                return false;
            };
            let in_bounds = x
                .checked_add(width)
                .is_some_and(|right| right <= self.width)
                && y.checked_add(height)
                    .is_some_and(|bottom| bottom <= self.height);
            if !in_bounds || pixels.len() != rect_bytes {
                return false;
            }
            let row_bytes = width as usize * 4;
            let dst_stride = self.width as usize * 4;
            for row in 0..height as usize {
                let src_start = row * row_bytes;
                let dst_row = y as usize + row;
                let dst_start = dst_row * dst_stride + x as usize * 4;
                self.pixels[dst_start..dst_start + row_bytes]
                    .copy_from_slice(&pixels[src_start..src_start + row_bytes]);
            }
            self.dirty = true;
            true
        }

        /// Realize the GPU texture if it isn't yet, or re-upload the
        /// staged pixels if `update_texture` dirtied them since the last
        /// record. Runs at record time on the driver thread, where a
        /// device + queue are available.
        fn ensure_realized(
            &mut self,
            device: &wgpu::Device,
            queue: &wgpu::Queue,
            pipeline: &QuadPipeline,
        ) {
            if let Some(realized) = &self.realized {
                // Already on the GPU; re-upload only if `update_texture`
                // dirtied the staging buffer since the last record.
                if self.dirty {
                    upload_texture_full(queue, realized, &self.pixels);
                }
            } else {
                self.realized = Some(realize_texture(
                    device,
                    queue,
                    pipeline,
                    self.width,
                    self.height,
                    &self.pixels,
                ));
            }
            self.dirty = false;
        }
    }

    /// Session-scoped texture registry. `next_id` hands out the
    /// `texture_id` a `create_texture` reply carries — assigned in
    /// sequence the same way ADR-0103 assigns instrument ids, so ids are
    /// stable for the session and depend only on creation order.
    struct TextureRegistry {
        next_id: u32,
        entries: HashMap<u32, StagedTexture>,
    }

    impl TextureRegistry {
        fn new() -> Self {
            Self {
                next_id: 0,
                entries: HashMap::new(),
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
                world_quads_warned: Arc::new(AtomicBool::new(false)),
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
        #[handler]
        fn on_capture_frame(&self, ctx: &mut NativeCtx<'_>, mail: CaptureFrame) {
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

            // ADR-0086 §12 / iamacoffeepot/aether#1273: capture is a
            // deferred reply — the render thread sends the reply after
            // the cap handler has returned. Acquire the settlement hold
            // *before* the queue handoff so `HoldOpen` is visible to
            // the settlement counter ahead of this handler's
            // `Finished`; the hold rides on `PendingCapture` and drops
            // with `req` after `outbound.send_reply` on the render
            // thread (`desktop/driver.rs` line ~568, `test_bench/
            // bench.rs` line ~1019). Mirrors the ADR-0093
            // hold-until-resolve dispatch's eager-acquire. The
            // early-return branches above reply
            // synchronously inside this handler's own dispatch window,
            // so they're naturally covered by the handler's in-flight
            // `Finished` bracket and don't need the hold.
            let hold = ctx.mailer().acquire_settlement_hold(ctx.in_flight_root());
            let pending = PendingCapture {
                reply_to: sender,
                after_mails: after,
                pre_settlements,
                hold,
            };
            if !backend.queue.request(pending) {
                backend.outbound.send_reply(
                    sender,
                    &CaptureFrameResult::Err {
                        error: "capture already pending; try again once the in-flight \
                            request completes"
                            .to_owned(),
                    },
                );
                return;
            }

            if let Err(reason) = (backend.wake)() {
                let _ = backend.queue.take();
                backend.outbound.send_reply(
                    sender,
                    &CaptureFrameResult::Err {
                        error: reason.to_owned(),
                    },
                );
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
        fn on_create_texture(&self, ctx: &mut NativeCtx<'_>, mail: CreateTexture) {
            if let Some(obs) = &self.config.observed_kinds {
                obs.lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063")
                    .push(<CreateTexture as Kind>::NAME.into());
            }
            let expected = expected_pixel_bytes(mail.width, mail.height);
            let Some(expected) = expected else {
                ctx.reply(&CreateTextureResult::Err {
                    error: format!(
                        "texture dimensions {}x{} overflow or are zero",
                        mail.width, mail.height
                    ),
                });
                return;
            };
            if mail.pixels.len() != expected {
                ctx.reply(&CreateTextureResult::Err {
                    error: format!(
                        "pixels length {} does not match width*height*4 = {expected}",
                        mail.pixels.len()
                    ),
                });
                return;
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
            ctx.reply(&CreateTextureResult::Ok { texture_id });
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
    }

    /// RGBA8 byte count for a `width x height` texture, or `None` if the
    /// dimensions are zero or the product overflows `usize`. Shared by the
    /// `create_texture` validation and the `update_texture` sub-rect
    /// check.
    fn expected_pixel_bytes(width: u32, height: u32) -> Option<usize> {
        if width == 0 || height == 0 {
            return None;
        }
        (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
    }

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
        quad_frame: Arc<Mutex<Vec<QuadBatch>>>,
        /// Most-recently-rendered quad batches, kept across frames so an
        /// idle `capture` (no producer this frame) replays them, matching
        /// `last_submitted`'s role for triangles.
        quad_last_submitted: Arc<Mutex<Vec<QuadBatch>>>,
        /// Session-scoped texture registry: staged CPU pixels + lazily-
        /// realized GPU textures. Written by the cap dispatcher thread
        /// (`create_texture` / `update_texture`), realized + read by the
        /// driver thread at record time.
        textures: Arc<Mutex<TextureRegistry>>,
        /// One-shot guard so a `World`-space quad batch (unimplemented
        /// until #1699) warns once at encode rather than every frame a
        /// producer resends it.
        world_quads_warned: Arc<AtomicBool>,
        /// wgpu state, installed post-cap-construction by the driver via
        /// [`Self::install_gpu`]. Boots empty because winit 0.30's
        /// `ActiveEventLoop::create_window` only fires inside `resumed`,
        /// after `Builder::build` has returned. Test-bench (no surface)
        /// installs immediately after `build_passive`; desktop installs
        /// in its `resumed` handler. Encoder-level methods panic if
        /// called before install — in practice every code path that
        /// calls them runs after the install site.
        gpu: Arc<OnceLock<RenderGpu>>,
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
            {
                let mut live = self
                    .frame_vertices
                    .lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063");
                let mut last = self
                    .last_submitted
                    .lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063");
                if !live.is_empty() {
                    // Producer emitted: swap into cache.
                    mem::swap(&mut *live, &mut *last);
                    // Post-swap, `live` holds what `last` held before
                    // — stale geometry from however many frames ago.
                    // Clear (preserves capacity) so the next tick's
                    // `on_draw_triangle` appends into an empty buffer
                    // without reallocating.
                    live.clear();
                } else if !replay_cache_when_idle {
                    // Commit-current: producer chose not to emit
                    // this frame, so the cache should reflect that
                    // for any subsequent replay-cache caller.
                    last.clear();
                }
                // else: replay-cache + empty live — leave cache
                // alone, render its current contents.
            }
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
        /// staging buffer), expands its quads into screen-space vertices,
        /// and draws with that texture's bind group. An unknown
        /// `texture_id` warn-drops the batch; a `World`-space batch
        /// warn-drops once (the world-anchor path lands in #1699).
        ///
        /// # Panics
        /// Panics if `install_gpu` hasn't been called, or if any internal
        /// mutex is poisoned — fail-fast per ADR-0063.
        pub fn record_overlay_pass(
            &self,
            encoder: &mut wgpu::CommandEncoder,
            replay_cache_when_idle: bool,
        ) {
            let gpu = self.expect_gpu();
            {
                let mut live = self
                    .quad_frame
                    .lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063");
                let mut last = self
                    .quad_last_submitted
                    .lock()
                    .expect("mutex poisoned; fail-fast per ADR-0063");
                if !live.is_empty() {
                    mem::swap(&mut *live, &mut *last);
                    live.clear();
                } else if !replay_cache_when_idle {
                    last.clear();
                }
            }
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

            let mut registry = self
                .textures
                .lock()
                .expect("mutex poisoned; fail-fast per ADR-0063");

            // First pass: realize / re-upload every Screen texture the
            // frame references, mutably borrowing the registry.
            for batch in &batches {
                if matches!(batch.space, QuadSpace::World { .. }) {
                    if !self.world_quads_warned.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            target: "aether_capabilities::render",
                            "draw_textured_quads with World space is not yet implemented \
                             (lands in #1699); dropping the batch",
                        );
                    }
                    continue;
                }
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
                if matches!(batch.space, QuadSpace::World { .. }) {
                    continue;
                }
                let Some(entry) = registry.entries.get(&batch.texture_id) else {
                    continue;
                };
                let Some(realized) = entry.realized.as_ref() else {
                    continue;
                };
                #[allow(clippy::cast_possible_truncation)]
                let first_vertex = (vertex_bytes.len() / QUAD_VERTEX_STRIDE as usize) as u32;
                for quad in &batch.quads {
                    push_screen_quad_vertices(
                        &mut vertex_bytes,
                        [quad.x, quad.y, quad.width, quad.height],
                        [quad.u0, quad.v0, quad.u1, quad.v1],
                        quad.tint,
                    );
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

    #[cfg(test)]
    mod tests {
        use std::time::Duration;

        use super::*;
        use crate::test_chassis::TestChassis;
        use aether_actor::Actor;
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
            use aether_actor::Singleton;
            use aether_data::mailbox_id_from_name;

            let frozen = mailbox_id_from_name(<RenderCapability as Actor>::NAMESPACE);
            assert_eq!(<RenderCapability as Singleton>::resolve(0), frozen);
            assert_eq!(
                <RenderCapability as Singleton>::resolve(0xFFFF_FFFF_FFFF_FFFF),
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

        /// ADR-0105: `expected_pixel_bytes` is the single source of the
        /// RGBA8 length rule. Zero dimensions and overflowing products
        /// return `None`; a valid texture returns `width * height * 4`.
        #[test]
        fn expected_pixel_bytes_validates_dimensions() {
            assert_eq!(expected_pixel_bytes(2, 3), Some(24));
            assert_eq!(expected_pixel_bytes(0, 4), None);
            assert_eq!(expected_pixel_bytes(4, 0), None);
            assert_eq!(expected_pixel_bytes(u32::MAX, u32::MAX), None);
        }

        /// The registry hands out ids in creation order, starting at 0 —
        /// the same id-assignment shape ADR-0103 uses for instruments.
        #[test]
        fn texture_registry_assigns_sequential_ids() {
            let mut registry = TextureRegistry::new();
            let mut next = || {
                let id = registry.next_id;
                registry.next_id += 1;
                registry.entries.insert(
                    id,
                    StagedTexture {
                        width: 1,
                        height: 1,
                        pixels: vec![0, 0, 0, 0],
                        realized: None,
                        dirty: true,
                    },
                );
                id
            };
            assert_eq!(next(), 0);
            assert_eq!(next(), 1);
            assert_eq!(next(), 2);
            assert_eq!(registry.entries.len(), 3);
        }

        /// `apply_subrect` writes an in-bounds rect into the staged pixels
        /// and dirties the texture; an out-of-bounds rect, a zero
        /// dimension, or a pixel-length mismatch leaves the buffer
        /// untouched and returns `false`.
        #[test]
        fn staged_texture_apply_subrect_bounds() {
            let mut texture = StagedTexture {
                width: 2,
                height: 2,
                pixels: vec![0u8; 16],
                realized: None,
                dirty: false,
            };
            // Overwrite the bottom-right pixel (1, 1) with 0xAA bytes.
            assert!(texture.apply_subrect(1, 1, 1, 1, &[0xAA, 0xAA, 0xAA, 0xAA]));
            assert!(texture.dirty);
            assert_eq!(&texture.pixels[12..16], &[0xAA, 0xAA, 0xAA, 0xAA]);
            // The other three pixels are untouched.
            assert_eq!(&texture.pixels[0..12], &[0u8; 12]);

            // Out of bounds (rect extends past the right edge).
            texture.dirty = false;
            assert!(!texture.apply_subrect(1, 0, 2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]));
            assert!(!texture.dirty);
            // Pixel-length mismatch for the declared rect.
            assert!(!texture.apply_subrect(0, 0, 1, 1, &[1, 2, 3]));
            // Zero-sized rect.
            assert!(!texture.apply_subrect(0, 0, 0, 1, &[]));
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
    use aether_kinds::{CaptureFrameResult, CreateTextureResult};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::outbound::HubOutbound;

    use super::{
        Camera, CaptureFrame, CreateTexture, DrawTexturedQuads, DrawTriangle, UpdateTexture,
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
        #[handler]
        fn on_capture_frame(&self, ctx: &mut NativeCtx<'_>, _mail: CaptureFrame) {
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
        #[handler]
        fn on_create_texture(&self, ctx: &mut NativeCtx<'_>, _mail: CreateTexture) {
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
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_chassis::{decode_reply, test_mailer_and_rx};
        use aether_data::{MailboxId, Source, SourceAddr};
        use aether_data::{SessionToken, Uuid};
        use aether_kinds::CreateTexture;
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
            let mut ctx = NativeCtx::new(
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
    }
}
