//! ADR-0070 phase 3 (Option B per ADR-0071 phase 4): render + camera
//! sinks as one native [`Capability`].
//!
//! Claims `aether.sink.render` and `aether.sink.camera`, spawns one
//! dispatcher thread per mailbox, and exposes the accumulated state
//! the chassis frame loop reads each tick:
//!
//! - `frame_vertices` — the consolidated vertex buffer (drained at
//!   frame boundary, refilled by inbound `aether.draw_triangle` mail);
//! - `triangles_rendered` — lifetime triangle counter;
//! - `camera_state` — latest `[f32; 16]` `view_proj` from any
//!   `aether.camera` mail.
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
//! `DriverCtx::expect::<RenderRunning>()` instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aether_kinds::DRAW_TRIANGLE_BYTES;

use crate::capability::{BootError, Capability, ChassisCtx, Envelope, RunningCapability};
use crate::render::{IDENTITY_VIEW_PROJ, Pipeline, Targets};

pub const RENDER_SINK_NAME: &str = "aether.sink.render";
pub const CAMERA_SINK_NAME: &str = "aether.sink.camera";

const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Configuration for [`RenderCapability`]. `vertex_buffer_bytes` is
/// the maximum bytes the render accumulator will hold before
/// truncating with a warn — desktop and test-bench both pass
/// [`crate::render::VERTEX_BUFFER_BYTES`].
///
/// `observed_kinds`, when set, has every inbound mail's kind name
/// pushed to it from both the render and camera dispatchers — used
/// by the in-process test-bench to assert what kinds the sinks
/// have seen. Production chassis leave it `None` (zero overhead).
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

/// Render + camera sink capability. Constructed before
/// [`Capability::boot`]; the accumulator `Arc`s are pre-allocated and
/// exposed via [`Self::handles`] so a chassis using the legacy
/// `boot.add_capability` path can capture them before the capability
/// moves into boot.
pub struct RenderCapability {
    config: RenderConfig,
    handles: RenderHandles,
}

/// Bundle of accumulator state shared between the capability's
/// dispatcher threads (write side) and the chassis frame loop (read
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
        Self { config, handles }
    }

    /// Pre-boot accessor for the accumulator state. Cloned references
    /// survive the move into [`Capability::boot`] so the chassis frame
    /// loop reads the same buffers the dispatcher threads write.
    pub fn handles(&self) -> RenderHandles {
        self.handles.clone()
    }
}

pub struct RenderRunning {
    handles: RenderHandles,
    render_thread: Option<JoinHandle<()>>,
    camera_thread: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    /// wgpu state, installed post-boot by the driver via
    /// [`Self::install_gpu`]. Boots empty because winit 0.30's
    /// `ActiveEventLoop::create_window` only fires inside `resumed`,
    /// after [`Capability::boot`] has returned. Test-bench (no
    /// surface) installs immediately after `build_passive`; desktop
    /// installs in its `resumed` handler. Encoder-level methods
    /// (added in C2) panic if called before install — in practice
    /// every code path that calls them runs after the install site.
    gpu: OnceLock<RenderGpu>,
}

/// Bundle of wgpu resources `RenderRunning` owns post-install.
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

impl RenderRunning {
    /// Post-boot accessor for the accumulator state. Used by drivers
    /// reading via the chassis_builder typed lookup once render
    /// migrates onto the new builder; the legacy
    /// `boot.add_capability` path uses [`RenderCapability::handles`]
    /// instead.
    pub fn handles(&self) -> RenderHandles {
        self.handles.clone()
    }

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
            .expect("RenderRunning::install_gpu called twice");
    }

    /// Returns the installed [`RenderGpu`], or `None` if `install_gpu`
    /// hasn't been called yet. Encoder-level methods (added in
    /// ADR-0071 phase C2) use this; today only the chassis-side glue
    /// reaches in.
    pub fn gpu(&self) -> Option<&RenderGpu> {
        self.gpu.get()
    }
}

impl Capability for RenderCapability {
    type Running = RenderRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let RenderCapability { config, handles } = self;

        let render_claim = ctx.claim_mailbox(RENDER_SINK_NAME)?;
        let camera_claim = ctx.claim_mailbox(CAMERA_SINK_NAME)?;

        let shutdown_flag = Arc::new(AtomicBool::new(false));

        let render_thread = {
            let frame_vertices = Arc::clone(&handles.frame_vertices);
            let triangles_rendered = Arc::clone(&handles.triangles_rendered);
            let cap = config.vertex_buffer_bytes;
            let receiver = render_claim.receiver;
            let flag = Arc::clone(&shutdown_flag);
            let observed = config.observed_kinds.clone();
            thread::Builder::new()
                .name("aether-render-sink".into())
                .spawn(move || {
                    while !flag.load(Ordering::Relaxed) {
                        match receiver.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                            Ok(env) => {
                                if let Some(obs) = &observed {
                                    obs.lock().unwrap().push(env.kind_name.clone());
                                }
                                dispatch_render_envelope(
                                    &frame_vertices,
                                    &triangles_rendered,
                                    cap,
                                    &env,
                                );
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .map_err(|e| BootError::Other(Box::new(e)))?
        };

        let camera_thread = {
            let camera_state = Arc::clone(&handles.camera_state);
            let receiver = camera_claim.receiver;
            let flag = Arc::clone(&shutdown_flag);
            let observed = config.observed_kinds.clone();
            thread::Builder::new()
                .name("aether-camera-sink".into())
                .spawn(move || {
                    while !flag.load(Ordering::Relaxed) {
                        match receiver.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                            Ok(env) => {
                                if let Some(obs) = &observed {
                                    obs.lock().unwrap().push(env.kind_name.clone());
                                }
                                dispatch_camera_envelope(&camera_state, &env);
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .map_err(|e| BootError::Other(Box::new(e)))?
        };

        Ok(RenderRunning {
            handles,
            render_thread: Some(render_thread),
            camera_thread: Some(camera_thread),
            shutdown_flag,
            gpu: OnceLock::new(),
        })
    }
}

impl RunningCapability for RenderRunning {
    fn shutdown(self: Box<Self>) {
        let RenderRunning {
            handles: _,
            mut render_thread,
            mut camera_thread,
            shutdown_flag,
            gpu: _,
        } = *self;
        shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(t) = render_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = camera_thread.take() {
            let _ = t.join();
        }
        // gpu drops here; wgpu device/queue/pipeline/targets clean up
        // via their own Drop impls.
    }
}

/// Render envelope dispatcher. Truncates the inbound vertex bytes at
/// the sink boundary so a single oversized mesh degrades gracefully
/// instead of collapsing the whole frame downstream. Rounds to whole
/// triangles so the GPU vertex buffer never sees a half-triangle.
fn dispatch_render_envelope(
    frame_vertices: &Mutex<Vec<u8>>,
    triangles_rendered: &AtomicU64,
    cap: usize,
    env: &Envelope,
) {
    let mut verts = frame_vertices.lock().unwrap();
    let available = cap.saturating_sub(verts.len());
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
            cap = cap,
            "render sink dropped triangles beyond fixed vertex buffer",
        );
    }
}

/// Camera envelope dispatcher. Latest-value-wins semantics: each
/// successful mail overwrites; on length mismatch or cast failure the
/// prior value stays. Initialised in [`RenderCapability::new`] to
/// [`IDENTITY_VIEW_PROJ`] so the first frame draws unchanged until a
/// camera component starts publishing.
fn dispatch_camera_envelope(camera_state: &Mutex<[f32; 16]>, env: &Envelope) {
    if env.payload.len() != 64 {
        tracing::warn!(
            target: "aether_substrate::camera",
            got = env.payload.len(),
            expected = 64,
            "camera sink: payload length mismatch, dropping",
        );
        return;
    }
    match bytemuck::try_pod_read_unaligned::<[f32; 16]>(&env.payload) {
        Ok(mat) => *camera_state.lock().unwrap() = mat,
        Err(e) => tracing::warn!(
            target: "aether_substrate::camera",
            error = %e,
            "camera sink: cast failed, dropping",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::mail::{KindId, ReplyTo};
    use crate::mailer::Mailer;
    use crate::registry::{MailboxEntry, Registry};

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    fn deliver(registry: &Registry, name: &str, payload: &[u8]) {
        let id = registry.lookup(name).expect("mailbox registered");
        let MailboxEntry::Sink(handler) = registry.entry(id).expect("entry exists") else {
            panic!("expected sink entry for {name}");
        };
        handler(KindId(0), "test.kind", None, ReplyTo::NONE, payload, 1);
    }

    #[test]
    fn capability_claims_render_and_camera_mailboxes() {
        let (registry, mailer) = fresh_substrate();
        let cap = RenderCapability::new(RenderConfig::default());
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect("build succeeds");
        assert_eq!(chassis.len(), 1);
        assert!(registry.lookup(RENDER_SINK_NAME).is_some());
        assert!(registry.lookup(CAMERA_SINK_NAME).is_some());
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
        deliver(&registry, RENDER_SINK_NAME, &one_triangle);

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
        deliver(&registry, RENDER_SINK_NAME, &payload);

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
    fn camera_dispatcher_writes_view_proj_on_well_formed_payload() {
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
        deliver(&registry, CAMERA_SINK_NAME, bytes);

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
    fn camera_dispatcher_drops_wrong_length_payload() {
        let (registry, mailer) = fresh_substrate();
        let cap = RenderCapability::new(RenderConfig::default());
        let handles = cap.handles();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect("build succeeds");

        // 16 bytes — wrong length, should be 64.
        deliver(&registry, CAMERA_SINK_NAME, &[1u8; 16]);

        std::thread::sleep(Duration::from_millis(50));
        // Identity unchanged.
        assert_eq!(*handles.camera_state.lock().unwrap(), IDENTITY_VIEW_PROJ);

        chassis.shutdown();
    }
}
