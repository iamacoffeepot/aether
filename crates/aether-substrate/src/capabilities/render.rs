//! Issue 545 PR E2: render cap converted onto the unified `#[actor]`
//! shape. Pre-PR-E2 render was the one cap still on the legacy
//! `Capability + boot()` path with hand-rolled thread spawn — every
//! other cap moved to `#[actor]` in PR E1. This PR closes the drift.
//!
//! `RenderCapability` is now a regular `#[actor]` block. Two
//! `#[handler]` methods (`on_draw_triangle`, `on_camera`) replace the
//! manual `dispatch_envelope` / `Capability::boot` machinery. The
//! chassis builder spawns the dispatcher thread via
//! `spawn_actor_dispatcher`, which detects `FRAME_BARRIER = true` and
//! claims through the frame-bound path so the chassis frame loop's
//! `drain_frame_bound_or_abort` can wait on the cap's `pending`
//! counter as before (ADR-0074 §Decision 5).
//!
//! Driver-side state (wgpu device, queue, pipeline, offscreen
//! targets, accumulator buffers) lives on [`RenderHandles`], which
//! the cap exposes via [`RenderCapability::handles`] **before** the
//! cap moves into the chassis builder. The driver retains an
//! `RenderHandles` clone (cheap — every field is Arc-shared) and
//! calls encoder-level methods on it. Pre-PR-E2 the driver retrieved
//! `Arc<RenderCapability>` via `DriverCtx::expect`; that path retires
//! for render — facade caps don't go through the typed runnings map.
//!
//! Phase 4 keeps the GPU lifecycle, encoder creation, and presentation
//! in the chassis driver — this capability owns only the mail surface
//! and accumulator state.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use aether_data::{Actor, Kind, Singleton};
use aether_kinds::{Camera, DRAW_TRIANGLE_BYTES, DrawTriangle};

use crate::render::{
    CaptureMeta, IDENTITY_VIEW_PROJ, Pipeline, RenderError, Targets, build_main_pipeline,
    finish_capture, prepare_capture_copy, record_main_pass,
};

/// Configuration for [`RenderCapability`]. `vertex_buffer_bytes` is
/// the maximum bytes the render accumulator will hold before
/// truncating with a warn — desktop and test-bench both pass
/// [`crate::render::VERTEX_BUFFER_BYTES`].
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
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            vertex_buffer_bytes: crate::render::VERTEX_BUFFER_BYTES,
            observed_kinds: None,
        }
    }
}

/// `aether.render` mailbox cap. Holds [`RenderHandles`] (the
/// driver-facing accumulator state plus GPU bundle) and the
/// per-instance config. Pre-extract the handles via [`Self::handles`]
/// before moving the cap into the chassis builder; the dispatcher
/// thread then owns the cap and routes `aether.draw_triangle` /
/// `aether.camera` mail through the macro-emitted `Dispatch` impl.
pub struct RenderCapability {
    handles: RenderHandles,
    config: RenderConfig,
}

impl RenderCapability {
    pub fn new(config: RenderConfig) -> Self {
        let handles = RenderHandles {
            frame_vertices: Arc::new(Mutex::new(Vec::<u8>::with_capacity(
                config.vertex_buffer_bytes,
            ))),
            triangles_rendered: Arc::new(AtomicU64::new(0)),
            camera_state: Arc::new(Mutex::new(IDENTITY_VIEW_PROJ)),
            gpu: Arc::new(OnceLock::new()),
        };
        Self { handles, config }
    }

    /// Cheap clone of the driver-facing handles bundle. Call this
    /// **before** `chassis.with(cap)` — once the cap moves into
    /// the dispatcher thread, the only access to its accumulator
    /// state is through the cloned handles.
    pub fn handles(&self) -> RenderHandles {
        self.handles.clone()
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
    /// dropping a triangle the component meant for this frame. The
    /// chassis builder's `spawn_actor_dispatcher` checks this const
    /// and claims through the frame-bound path so the cap's `pending`
    /// counter registers in the chassis's `frame_bound_pending` Vec.
    const FRAME_BARRIER: bool = true;
}

impl Singleton for RenderCapability {}

#[aether_data::actor]
impl RenderCapability {
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
    #[aether_data::handler]
    fn on_draw_triangle(&mut self, mails: &[DrawTriangle]) {
        if let Some(obs) = &self.config.observed_kinds {
            obs.lock()
                .unwrap()
                .push(<DrawTriangle as Kind>::NAME.into());
        }
        let bytes: &[u8] = bytemuck::cast_slice(mails);
        let cap_bytes = self.config.vertex_buffer_bytes;
        let mut verts = self.handles.frame_vertices.lock().unwrap();
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
    /// Initialised in [`RenderCapability::new`] to
    /// [`IDENTITY_VIEW_PROJ`] so the first frame draws unchanged
    /// until a camera component starts publishing.
    ///
    /// # Agent
    /// Camera components mail `aether.camera { view_proj: [f32; 16] }`
    /// to this mailbox. Fire-and-forget; latest value wins.
    #[aether_data::handler]
    fn on_camera(&mut self, mail: Camera) {
        if let Some(obs) = &self.config.observed_kinds {
            obs.lock().unwrap().push(<Camera as Kind>::NAME.into());
        }
        *self.handles.camera_state.lock().unwrap() = mail.view_proj;
    }
}

/// Bundle of accumulator state plus GPU resources, shared between
/// the cap's dispatcher thread (write side for accumulators) and the
/// chassis driver (read side for accumulators, install + read for
/// GPU). All fields are `Arc`s so cloning is cheap and shutdown
/// drops are independent.
#[derive(Clone)]
pub struct RenderHandles {
    pub frame_vertices: Arc<Mutex<Vec<u8>>>,
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
    /// `build_passive` returns. Calling twice panics: install is the
    /// chassis's promise that wgpu state is now ready and stable for
    /// the chassis lifetime.
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
    pub fn gpu(&self) -> Option<&RenderGpu> {
        self.gpu.get()
    }

    fn expect_gpu(&self) -> &RenderGpu {
        self.gpu.get().expect(
            "RenderHandles::install_gpu must be called before encoder-level methods. \
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
        let cap = self.frame_vertices.lock().unwrap().capacity();
        let vertices = std::mem::replace(
            &mut *self.frame_vertices.lock().unwrap(),
            Vec::with_capacity(cap),
        );
        let view_proj = *self.camera_state.lock().unwrap();
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

        // Send 5 triangles' worth of bytes in one batched mail; only
        // 2 fit. The slice-typed handler reads the whole batch and
        // truncates at the cap boundary.
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

        // 16 bytes — wrong length, decode fails, macro miss path
        // logs warn at chassis-side dispatcher; identity unchanged.
        deliver(
            &registry,
            RenderCapability::NAMESPACE,
            <Camera as Kind>::ID,
            &[1u8; 16],
        );

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(*handles.camera_state.lock().unwrap(), IDENTITY_VIEW_PROJ);

        chassis.shutdown();
    }
}
