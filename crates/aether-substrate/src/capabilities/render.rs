//! ADR-0070 phase 3 (Option B per ADR-0071 phase 4) + ADR-0074
//! §Decision 7: render is one native [`Capability`] with one mailbox
//! and one dispatcher thread. The pre-Phase-3 `aether.sink.camera`
//! mailbox folded into `aether.render`; the camera kind
//! (`aether.camera`) keeps its name but its recipient is now the
//! render mailbox.
//!
//! The dispatcher pulls envelopes off a [`crate::FrameBoundClaim`]
//! inbox and routes by kind id:
//!
//! - `aether.draw_triangle` → append vertex bytes to `frame_vertices`,
//!   bump `triangles_rendered` (with the same fixed-buffer truncation
//!   semantics as before).
//! - `aether.camera` → overwrite the cached `view_proj` (latest-value-
//!   wins; init'd to [`crate::render::IDENTITY_VIEW_PROJ`]).
//!
//! Frame-bound (`Capability::FRAME_BARRIER = true`): the chassis frame
//! loop calls [`crate::frame_loop::drain_frame_bound_or_abort`] each
//! frame so render's inbox quiesces before the chassis driver records
//! the GPU pass. The pending counter rides on `FrameBoundClaim` —
//! incremented by the sink registration handler, decremented by the
//! dispatcher after each `dispatch_envelope` call.
//!
//! Phase 4 keeps the GPU lifecycle, encoder creation, and presentation
//! in the chassis driver — this capability owns only the mail surface
//! and accumulator state. Encoder-level primitives + GPU bring-up are
//! deferred to a future phase once a second consumer (test-bench
//! `PassiveChassis` build with no winit) makes the right shape obvious.
//!
//! The capability builder exposes [`RenderCapability::handles`] so a
//! chassis adding it via the legacy `boot.add_capability` path can
//! pull the accumulator `Arc`s before the capability's `boot()` moves
//! `self`. Once chassis composition migrates to the chassis_builder
//! `Builder` (ADR-0071), drivers will read the same `Arc`s through
//! `DriverCtx::expect::<RenderCapability>()` instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use aether_actor::Actor;
use aether_data::Kind;
use aether_kinds::{Camera, DRAW_TRIANGLE_BYTES, DrawTriangle};

use crate::capability::{BootError, Capability, ChassisCtx, Envelope, SinkSender};
use crate::render::{
    CaptureMeta, IDENTITY_VIEW_PROJ, Pipeline, RenderError, Targets, build_main_pipeline,
    finish_capture, prepare_capture_copy, record_main_pass,
};

/// Configuration for [`RenderCapability`]. `vertex_buffer_bytes` is
/// the maximum bytes the render accumulator will hold before
/// truncating with a warn — desktop and test-bench both pass
/// [`crate::render::VERTEX_BUFFER_BYTES`].
///
/// `observed_kinds`, when set, has every inbound mail's kind name
/// pushed to it from the unified render dispatcher — used by the
/// in-process test-bench to assert what kinds the sink has seen.
/// Production chassis leave it `None` (zero overhead).
#[derive(Clone)]
pub struct RenderConfig {
    pub vertex_buffer_bytes: usize,
    pub observed_kinds: Option<Arc<Mutex<Vec<String>>>>,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            vertex_buffer_bytes: crate::render::VERTEX_BUFFER_BYTES,
            observed_kinds: None,
        }
    }
}

/// Render sink capability. Constructed before [`Capability::boot`];
/// the accumulator `Arc`s are pre-allocated so the chassis frame
/// loop and the dispatcher thread share the same backing storage.
///
/// Post-issue-525-Phase-2 the cap is one struct: pre-boot config
/// (`config`, `handles`) lives alongside the runtime fields populated
/// in `boot` (`thread`, `sink_sender`, `gpu`). The encoder-level
/// methods (`install_gpu`, `record_frame`, `record_capture_copy`,
/// etc.) live as inherent methods directly on `RenderCapability` —
/// drivers retrieve the cap via `DriverCtx::expect::<RenderCapability>()`
/// and call those methods on the returned `Arc<RenderCapability>`.
pub struct RenderCapability {
    config: RenderConfig,
    handles: RenderHandles,
    thread: Option<JoinHandle<()>>,
    sink_sender: Option<SinkSender>,
    /// wgpu state, installed post-boot by the driver via
    /// [`Self::install_gpu`]. Boots empty because winit 0.30's
    /// `ActiveEventLoop::create_window` only fires inside `resumed`,
    /// after [`Capability::boot`] has returned. Test-bench (no
    /// surface) installs immediately after `build_passive`; desktop
    /// installs in its `resumed` handler. Encoder-level methods
    /// panic if called before install — in practice every code path
    /// that calls them runs after the install site.
    gpu: OnceLock<RenderGpu>,
}

/// Bundle of accumulator state shared between the capability's
/// dispatcher thread (write side) and the chassis frame loop (read
/// side). All fields are `Arc`s so cloning is cheap and shutdown
/// drops are independent.
#[derive(Clone)]
pub struct RenderHandles {
    pub frame_vertices: Arc<Mutex<Vec<u8>>>,
    pub triangles_rendered: Arc<AtomicU64>,
    pub camera_state: Arc<Mutex<[f32; 16]>>,
}

impl RenderCapability {
    pub fn new(config: RenderConfig) -> Self {
        let handles = RenderHandles {
            frame_vertices: Arc::new(Mutex::new(Vec::<u8>::with_capacity(
                config.vertex_buffer_bytes,
            ))),
            triangles_rendered: Arc::new(AtomicU64::new(0)),
            camera_state: Arc::new(Mutex::new(IDENTITY_VIEW_PROJ)),
        };
        Self {
            config,
            handles,
            thread: None,
            sink_sender: None,
            gpu: OnceLock::new(),
        }
    }

    /// Pre-boot accessor for the accumulator state. Cloned references
    /// survive the move into [`Capability::boot`] so the chassis frame
    /// loop reads the same buffers the dispatcher thread writes.
    pub fn handles(&self) -> RenderHandles {
        self.handles.clone()
    }
}

/// Bundle of wgpu resources `RenderCapability` owns post-install.
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
    /// given size and pass [`Self`] to [`RenderCapability::install_gpu`].
    /// `polygon_mode` is `Fill` for the normal case; desktop's
    /// `AETHER_WIREFRAME=line` chassis env passes `Line` so the main
    /// pipeline draws as wireframe instead of building a separate
    /// overlay pipeline.
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

impl RenderCapability {
    /// Install the wgpu resources the encoder-level methods read. The
    /// driver constructs [`RenderGpu`] once it has a device + queue —
    /// for desktop that's inside `resumed` after winit hands back a
    /// window and surface; for test-bench it's right after
    /// `build_passive` returns. Calling twice panics: install is the
    /// chassis's promise that wgpu state is now ready and stable for
    /// the chassis lifetime.
    pub fn install_gpu(&self, gpu: RenderGpu) {
        self.gpu
            .set(gpu)
            .ok()
            .expect("RenderCapability::install_gpu called twice");
    }

    /// Returns the installed [`RenderGpu`], or `None` if `install_gpu`
    /// hasn't been called yet. Chassis-side glue that needs raw
    /// access to the pipeline's bind group layouts (e.g. desktop's
    /// wireframe overlay pipeline construction) reaches in here.
    pub fn gpu(&self) -> Option<&RenderGpu> {
        self.gpu.get()
    }

    fn expect_gpu(&self) -> &RenderGpu {
        self.gpu.get().expect(
            "RenderCapability::install_gpu must be called before encoder-level methods. \
             Desktop installs in winit's resumed; test-bench installs after build_passive.",
        )
    }

    /// Drain the current frame's accumulated vertices, read the
    /// latest camera view-proj, and record the main render pass into
    /// `encoder`. Each call consumes the accumulator (subsequent
    /// inbound mail builds the next frame). `extra_pipelines` are
    /// drawn after the main pipeline inside the same render pass —
    /// desktop passes a wireframe overlay pipeline here when
    /// `AETHER_WIREFRAME=overlay`; test-bench passes `&[]`.
    pub fn record_frame(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        extra_pipelines: &[&wgpu::RenderPipeline],
    ) -> Result<(), RenderError> {
        let gpu = self.expect_gpu();
        let cap = self.handles.frame_vertices.lock().unwrap().capacity();
        let vertices = std::mem::replace(
            &mut *self.handles.frame_vertices.lock().unwrap(),
            Vec::with_capacity(cap),
        );
        let view_proj = *self.handles.camera_state.lock().unwrap();
        let targets = gpu.targets.lock().unwrap();
        record_main_pass(
            &gpu.queue,
            encoder,
            &gpu.pipeline,
            &targets,
            &vertices,
            &view_proj,
            extra_pipelines,
        )
    }

    /// Encode a copy of the offscreen color target into a readback
    /// buffer. Pair with [`Self::finish_capture`] after submit. The
    /// readback buffer is reallocated on size mismatch with the
    /// current offscreen, so any sequence of resize → record_frame →
    /// record_capture_copy → submit → finish_capture works.
    pub fn record_capture_copy(&self, encoder: &mut wgpu::CommandEncoder) -> CaptureMeta {
        let gpu = self.expect_gpu();
        let mut targets = gpu.targets.lock().unwrap();
        prepare_capture_copy(&gpu.device, &mut targets, encoder)
    }

    /// Map the readback buffer prepared by [`Self::record_capture_copy`]
    /// and return the encoded PNG. Call after the encoder containing
    /// the matching `record_capture_copy` has been submitted.
    pub fn finish_capture(&self, meta: &CaptureMeta) -> Result<Vec<u8>, String> {
        let gpu = self.expect_gpu();
        let targets = gpu.targets.lock().unwrap();
        finish_capture(&gpu.device, &targets, meta)
    }

    /// Resize the offscreen color + depth targets. Idempotent on
    /// zero dimensions (matches winit's `Resized(0, 0)` on minimize).
    pub fn resize(&self, width: u32, height: u32) {
        let gpu = self.expect_gpu();
        let mut targets = gpu.targets.lock().unwrap();
        targets.resize(&gpu.device, width, height);
    }

    /// Cloned `Arc<wgpu::Device>`. Drivers that need the device for
    /// their own pipelines (e.g. desktop's wireframe overlay pipeline,
    /// swapchain blit) clone here.
    pub fn device(&self) -> Arc<wgpu::Device> {
        Arc::clone(&self.expect_gpu().device)
    }

    /// Cloned `Arc<wgpu::Queue>`. Drivers submit through this; the
    /// shared queue means render's `record_frame` writes and the
    /// driver's swapchain submit go through the same submission
    /// order.
    pub fn queue(&self) -> Arc<wgpu::Queue> {
        Arc::clone(&self.expect_gpu().queue)
    }

    /// Format the offscreen color target was created with. Capture's
    /// BGRA-vs-RGBA decision keys on this; desktop's swapchain blit
    /// matches its surface format against this to pick a direct copy
    /// vs a manual swizzle.
    pub fn color_format(&self) -> wgpu::TextureFormat {
        self.expect_gpu().color_format
    }

    /// Current offscreen color target dimensions. Drivers reading
    /// after a `resize` see the new dimensions immediately.
    pub fn color_size(&self) -> (u32, u32) {
        let targets = self.expect_gpu().targets.lock().unwrap();
        (targets.width(), targets.height())
    }

    /// Run `f` with a borrow of the offscreen color texture. Used by
    /// desktop's swapchain blit: the closure body holds the targets
    /// mutex, so any encoder commands recorded inside are sequenced
    /// against any concurrent resize. Test-bench reaches the
    /// offscreen via the capture path and doesn't need this.
    pub fn with_color_texture<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&wgpu::Texture) -> R,
    {
        let gpu = self.expect_gpu();
        let targets = gpu.targets.lock().unwrap();
        f(targets.color_texture())
    }
}

impl Actor for RenderCapability {
    /// Components mail `aether.draw_triangle` and `aether.camera`
    /// (kind ids) to this mailbox; the GPU recorder pulls from here.
    /// The `aether.<name>` form is the post-ADR-0074 Phase 5
    /// convention for chassis-owned mailboxes; ADR-0074 §Decision 7
    /// folded the camera mailbox into render under this name.
    const NAMESPACE: &'static str = "aether.render";

    /// Render is the one chassis-owned actor that participates in the
    /// per-frame drain barrier (ADR-0074 §Decision 7). Without this,
    /// a `DrawTriangle` mail in flight when the chassis driver records
    /// the frame would land in the *next* frame's `frame_vertices`,
    /// dropping a triangle the component meant for this frame.
    const FRAME_BARRIER: bool = true;
}

impl Capability for RenderCapability {
    fn boot(mut self, ctx: &mut ChassisCtx<'_>) -> Result<Self, BootError> {
        let claim = ctx.claim_frame_bound_mailbox::<Self>()?;

        let frame_vertices = Arc::clone(&self.handles.frame_vertices);
        let triangles_rendered = Arc::clone(&self.handles.triangles_rendered);
        let camera_state = Arc::clone(&self.handles.camera_state);
        let cap_bytes = self.config.vertex_buffer_bytes;
        let observed = self.config.observed_kinds.clone();
        let pending = Arc::clone(&claim.pending);
        let receiver = claim.receiver;

        let thread = thread::Builder::new()
            .name("aether-render-sink".into())
            .spawn(move || {
                // Channel-drop + join: the sender lives on
                // `RenderCapability.sink_sender`; shutdown drops it,
                // disconnecting the channel so `recv()` returns
                // `Err(Disconnected)` and the loop exits.
                while let Ok(env) = receiver.recv() {
                    if let Some(obs) = &observed {
                        obs.lock().unwrap().push(env.kind_name.clone());
                    }
                    dispatch_envelope(
                        &frame_vertices,
                        &triangles_rendered,
                        &camera_state,
                        cap_bytes,
                        &env,
                    );
                    // Decrement matches the sink-handler's increment —
                    // the chassis frame-bound drain barrier
                    // (`drain_frame_bound_or_abort`) reads this counter
                    // to know when the dispatcher is caught up.
                    pending.fetch_sub(1, Ordering::AcqRel);
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        self.thread = Some(thread);
        self.sink_sender = Some(claim.sink_sender);
        Ok(self)
    }
}

impl Drop for RenderCapability {
    fn drop(&mut self) {
        // Drop the strong sender to break the channel.
        self.sink_sender.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // gpu drops here; wgpu device/queue/pipeline/targets clean up
        // via their own Drop impls.
    }
}

/// Dispatch one envelope to the right per-kind handler. Routes by
/// kind id — `aether.draw_triangle` builds the frame vertex buffer,
/// `aether.camera` updates the cached `view_proj`. Unknown kinds
/// warn-drop without affecting either accumulator.
fn dispatch_envelope(
    frame_vertices: &Mutex<Vec<u8>>,
    triangles_rendered: &AtomicU64,
    camera_state: &Mutex<[f32; 16]>,
    cap_bytes: usize,
    env: &Envelope,
) {
    if env.kind == <DrawTriangle as Kind>::ID {
        dispatch_draw_triangle(frame_vertices, triangles_rendered, cap_bytes, env);
    } else if env.kind == <Camera as Kind>::ID {
        dispatch_camera(camera_state, env);
    } else {
        tracing::warn!(
            target: "aether_substrate::render",
            kind = %env.kind,
            kind_name = %env.kind_name,
            "render sink received unknown kind — dropping",
        );
    }
}

/// DrawTriangle handler. Truncates the inbound vertex bytes at the
/// sink boundary so a single oversized mesh degrades gracefully
/// instead of collapsing the whole frame downstream. Rounds to whole
/// triangles so the GPU vertex buffer never sees a half-triangle.
fn dispatch_draw_triangle(
    frame_vertices: &Mutex<Vec<u8>>,
    triangles_rendered: &AtomicU64,
    cap_bytes: usize,
    env: &Envelope,
) {
    let mut verts = frame_vertices.lock().unwrap();
    let available = cap_bytes.saturating_sub(verts.len());
    let write_len = env.payload.len().min(available);
    let write_len = write_len - (write_len % DRAW_TRIANGLE_BYTES);
    if write_len > 0 {
        verts.extend_from_slice(&env.payload[..write_len]);
        triangles_rendered.fetch_add((write_len / DRAW_TRIANGLE_BYTES) as u64, Ordering::Relaxed);
    }
    if write_len < env.payload.len() {
        tracing::warn!(
            target: "aether_substrate::render",
            accepted_bytes = write_len,
            dropped_bytes = env.payload.len() - write_len,
            cap = cap_bytes,
            "render sink dropped triangles beyond fixed vertex buffer",
        );
    }
}

/// Camera handler. Latest-value-wins semantics: each successful mail
/// overwrites; on length mismatch or cast failure the prior value
/// stays. Initialised in [`RenderCapability::new`] to
/// [`IDENTITY_VIEW_PROJ`] so the first frame draws unchanged until a
/// camera component starts publishing.
fn dispatch_camera(camera_state: &Mutex<[f32; 16]>, env: &Envelope) {
    if env.payload.len() != 64 {
        tracing::warn!(
            target: "aether_substrate::camera",
            got = env.payload.len(),
            expected = 64,
            "camera mail: payload length mismatch, dropping",
        );
        return;
    }
    match bytemuck::try_pod_read_unaligned::<[f32; 16]>(&env.payload) {
        Ok(mat) => *camera_state.lock().unwrap() = mat,
        Err(e) => tracing::warn!(
            target: "aether_substrate::camera",
            error = %e,
            "camera mail: cast failed, dropping",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::mail::{KindId, ReplyTo};
    use crate::mailer::Mailer;
    use crate::registry::{MailboxEntry, Registry};

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    fn deliver(registry: &Registry, name: &str, kind: KindId, payload: &[u8]) {
        let id = registry.lookup(name).expect("mailbox registered");
        let MailboxEntry::Sink(handler) = registry.entry(id).expect("entry exists") else {
            panic!("expected sink entry for {name}");
        };
        handler(kind, "test.kind", None, ReplyTo::NONE, payload, 1);
    }

    #[test]
    fn capability_claims_render_mailbox_only() {
        let (registry, mailer) = fresh_substrate();
        let cap = RenderCapability::new(RenderConfig::default());
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect("build succeeds");
        assert_eq!(chassis.len(), 1);
        assert!(registry.lookup(RenderCapability::NAMESPACE).is_some());
        // Camera mailbox retired (ADR-0074 §Decision 7).
        assert!(registry.lookup("aether.sink.camera").is_none());
    }

    #[test]
    fn render_dispatcher_appends_triangles_to_frame_vertices() {
        let (registry, mailer) = fresh_substrate();
        let cap = RenderCapability::new(RenderConfig::default());
        let handles = cap.handles();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect("build succeeds");

        let one_triangle = vec![0u8; DRAW_TRIANGLE_BYTES];
        deliver(
            &registry,
            RenderCapability::NAMESPACE,
            <DrawTriangle as Kind>::ID,
            &one_triangle,
        );

        // Wait briefly for the dispatcher thread to drain.
        for _ in 0..50 {
            if handles.triangles_rendered.load(Ordering::Relaxed) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(handles.triangles_rendered.load(Ordering::Relaxed), 1);
        assert_eq!(
            handles.frame_vertices.lock().unwrap().len(),
            DRAW_TRIANGLE_BYTES
        );

        chassis.shutdown();
    }

    #[test]
    fn render_sink_truncates_oversized_payloads_to_whole_triangles() {
        let (registry, mailer) = fresh_substrate();
        let cap = RenderCapability::new(RenderConfig {
            vertex_buffer_bytes: DRAW_TRIANGLE_BYTES * 2,
            ..RenderConfig::default()
        });
        let handles = cap.handles();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect("build succeeds");

        // Send 5 triangles; only 2 fit.
        let payload = vec![0u8; DRAW_TRIANGLE_BYTES * 5];
        deliver(
            &registry,
            RenderCapability::NAMESPACE,
            <DrawTriangle as Kind>::ID,
            &payload,
        );

        for _ in 0..50 {
            if handles.triangles_rendered.load(Ordering::Relaxed) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(handles.triangles_rendered.load(Ordering::Relaxed), 2);
        assert_eq!(
            handles.frame_vertices.lock().unwrap().len(),
            DRAW_TRIANGLE_BYTES * 2
        );

        chassis.shutdown();
    }

    #[test]
    fn camera_kind_writes_view_proj_on_well_formed_payload() {
        let (registry, mailer) = fresh_substrate();
        let cap = RenderCapability::new(RenderConfig::default());
        let handles = cap.handles();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect("build succeeds");

        // Identity scaled by 2.0 to detect the write.
        let mat: [f32; 16] = [
            2.0, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let bytes: &[u8] = bytemuck::cast_slice(&mat);
        deliver(
            &registry,
            RenderCapability::NAMESPACE,
            <Camera as Kind>::ID,
            bytes,
        );

        for _ in 0..50 {
            if handles.camera_state.lock().unwrap()[0] == 2.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*handles.camera_state.lock().unwrap(), mat);

        chassis.shutdown();
    }

    #[test]
    fn camera_kind_drops_wrong_length_payload() {
        let (registry, mailer) = fresh_substrate();
        let cap = RenderCapability::new(RenderConfig::default());
        let handles = cap.handles();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect("build succeeds");

        // 16 bytes — wrong length, should be 64.
        deliver(
            &registry,
            RenderCapability::NAMESPACE,
            <Camera as Kind>::ID,
            &[1u8; 16],
        );

        std::thread::sleep(Duration::from_millis(50));
        // Identity unchanged.
        assert_eq!(*handles.camera_state.lock().unwrap(), IDENTITY_VIEW_PROJ);

        chassis.shutdown();
    }
}
