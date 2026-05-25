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
use aether_kinds::{Camera, CaptureFrame, DrawTriangle};

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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    use aether_actor::actor;
    use aether_data::Kind;
    use aether_kinds::{CaptureFrameResult, DRAW_TRIANGLE_BYTES};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::capture::{CaptureQueue, PendingCapture};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::helpers::resolve_bundle;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::render::{
        CaptureMeta, IDENTITY_VIEW_PROJ, Pipeline, RenderError, Targets, build_main_pipeline,
        finish_capture, prepare_capture_copy, record_main_pass,
    };

    use super::{Camera, CaptureFrame, DrawTriangle};
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

            let pending = PendingCapture {
                reply_to: sender,
                after_mails: after,
                pre_settlements,
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
            let targets = Targets::new(&device, color_format, width, height);
            Self {
                device,
                queue,
                pipeline,
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
        use aether_substrate::mail::{KindId, ReplyTo};
        use std::thread;

        use crate::test_chassis::fresh_substrate;

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
            handler.enqueue(OwnedDispatch {
                kind,
                kind_name: "test.kind".to_owned(),
                origin: None,
                sender: ReplyTo::NONE,
                payload: MailRef::from(payload.to_vec()),
                count: 1,
                mail_id: MailId::NONE,
                root: MailId::NONE,
                parent_mail: None,
                t_enqueue: Nanos(0),
                enqueue_depth: 0,
            });
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
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::outbound::HubOutbound;

    use super::{Camera, CaptureFrame, DrawTriangle};
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
    }
}
