//! Shared chassis sink builders (issue 428).
//!
//! Desktop and test-bench register byte-for-byte identical render and
//! camera sinks; this module is the extraction. Each builder returns
//! the chassis-side state (vertex buffer, triangle counter, view-proj
//! matrix) plus the [`SinkHandler`] closure that writes into it. The
//! chassis owns the `Arc`-shared state for its frame loop to read; the
//! handler owns the closure that updates it under mail dispatch.
//!
//! Gated on the `render` feature alongside [`crate::render`] — only
//! chassis that draw pull these in. Headless keeps its 4-line nop
//! closures inline so the warn-suppressors are self-documenting.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_kinds::DRAW_TRIANGLE_BYTES;
use aether_mail::KindId;

use crate::registry::SinkHandler;
use crate::render::IDENTITY_VIEW_PROJ;

/// State the desktop / test-bench frame loop reads from each frame.
/// `frame_vertices` is the consolidated vertex buffer (drained at
/// frame boundary, refilled by inbound `aether.draw_triangle` mail);
/// `triangles_rendered` is the lifetime counter the hub's frame-stats
/// observation reads.
pub struct RenderAccumulator {
    pub frame_vertices: Arc<Mutex<Vec<u8>>>,
    pub triangles_rendered: Arc<AtomicU64>,
}

/// Build the `aether.sink.render` handler shared between desktop and
/// test-bench. `cap` is the maximum bytes the accumulator will hold
/// before truncating with a warn — both chassis pass
/// [`crate::render::VERTEX_BUFFER_BYTES`].
///
/// The returned handler is ready to hand to
/// `Registry::register_sink("aether.sink.render", handler)`. Hold the
/// returned [`RenderAccumulator`] for the frame loop to drain.
pub fn build_render_sink(cap: usize) -> (RenderAccumulator, SinkHandler) {
    let frame_vertices = Arc::new(Mutex::new(Vec::<u8>::with_capacity(cap)));
    let triangles_rendered = Arc::new(AtomicU64::new(0));
    let verts_for_sink = Arc::clone(&frame_vertices);
    let tris_for_sink = Arc::clone(&triangles_rendered);
    let handler: SinkHandler = Arc::new(
        move |_kind_id: KindId,
              _kind_name: &str,
              _origin: Option<&str>,
              _sender,
              bytes: &[u8],
              _count: u32| {
            // Truncate at the sink boundary so a single oversized
            // mesh degrades gracefully instead of collapsing the
            // whole frame downstream. Round to whole triangles so
            // the GPU vertex buffer never sees a half-triangle.
            let mut verts = verts_for_sink.lock().unwrap();
            let available = cap.saturating_sub(verts.len());
            let write_len = bytes.len().min(available);
            let write_len = write_len - (write_len % DRAW_TRIANGLE_BYTES);
            if write_len > 0 {
                verts.extend_from_slice(&bytes[..write_len]);
                tris_for_sink
                    .fetch_add((write_len / DRAW_TRIANGLE_BYTES) as u64, Ordering::Relaxed);
            }
            if write_len < bytes.len() {
                tracing::warn!(
                    target: "aether_substrate::render",
                    accepted_bytes = write_len,
                    dropped_bytes = bytes.len() - write_len,
                    cap = cap,
                    "render sink dropped triangles beyond fixed vertex buffer",
                );
            }
        },
    );
    (
        RenderAccumulator {
            frame_vertices,
            triangles_rendered,
        },
        handler,
    )
}

/// Build the `aether.sink.camera` handler shared between desktop and
/// test-bench. Returns the `Arc<Mutex<[f32; 16]>>` the frame loop
/// reads each tick to upload the latest view-proj to the GPU uniform,
/// plus the handler that decodes inbound `aether.camera` mail into it.
///
/// Latest-value-wins semantics: each successful mail overwrites; on
/// length mismatch or cast failure the prior value stays. Initialises
/// to [`IDENTITY_VIEW_PROJ`] so the first frame draws unchanged until
/// a camera component starts publishing.
pub fn build_camera_sink() -> (Arc<Mutex<[f32; 16]>>, SinkHandler) {
    let camera_state = Arc::new(Mutex::new(IDENTITY_VIEW_PROJ));
    let cam_for_sink = Arc::clone(&camera_state);
    let handler: SinkHandler = Arc::new(
        move |_kind_id: KindId,
              _kind_name: &str,
              _origin: Option<&str>,
              _sender,
              bytes: &[u8],
              _count: u32| {
            if bytes.len() != 64 {
                tracing::warn!(
                    target: "aether_substrate::camera",
                    got = bytes.len(),
                    expected = 64,
                    "camera sink: payload length mismatch, dropping",
                );
                return;
            }
            match bytemuck::try_pod_read_unaligned::<[f32; 16]>(bytes) {
                Ok(mat) => *cam_for_sink.lock().unwrap() = mat,
                Err(e) => tracing::warn!(
                    target: "aether_substrate::camera",
                    error = %e,
                    "camera sink: cast failed, dropping",
                ),
            }
        },
    );
    (camera_state, handler)
}
