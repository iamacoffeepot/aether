//! Cross-thread capture machinery for the `aether.render` cap
//! (iamacoffeepot/aether#1758 / #1780). The cap dispatcher thread
//! doesn't own the wgpu `Device` — it parks a resolved request on the
//! [`CaptureBackend`] queue for the chassis main loop to read back, and
//! reads the optional reference PNG synchronously off the render hot
//! path via [`resolve_reference`].

use std::fs;
use std::path::Path;
use std::sync::Arc;

use aether_kinds::SimilarityCheck;
use aether_substrate::capture::{CaptureQueue, ReferenceCapture};
use aether_substrate::mail::outbound::HubOutbound;

/// Per-chassis plumbing the [`super::RenderCapability`] capture handler
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

/// Resolve the optional reference image for a `#1780` similarity
/// check, reading it synchronously on the cap dispatcher thread so all
/// filesystem I/O stays off the render hot path. `Ok(None)` when no
/// check was requested; `Err(message)` when the reference can't be
/// used (unsupported namespace, no assets dir, forbidden path, or an
/// unreadable file) — the caller replies that message as
/// `CaptureFrameResult::Err`.
pub(super) fn resolve_reference(
    assets_dir: Option<&Path>,
    similarity: Option<&SimilarityCheck>,
) -> Result<Option<ReferenceCapture>, String> {
    let Some(sim) = similarity else {
        return Ok(None);
    };
    // Only the "assets" namespace is supported in v1.
    if sim.namespace != "assets" {
        return Err(format!(
            "capture_frame similarity: namespace {:?} is not supported in v1 — use \"assets\"",
            sim.namespace,
        ));
    }
    let Some(assets_dir) = assets_dir else {
        return Err(
            "capture_frame similarity: no assets directory is configured on this \
                    chassis; similarity checks are unavailable"
                .to_owned(),
        );
    };
    // Reject path components that would escape the assets root
    // (mirrors `LocalFileAdapter::resolve`).
    if sim.reference_path.starts_with('/') || sim.reference_path.split('/').any(|c| c == "..") {
        return Err(format!(
            "capture_frame similarity: reference_path {:?} is forbidden (contains '..' or \
             starts with '/')",
            sim.reference_path,
        ));
    }
    let full_path = assets_dir.join(&sim.reference_path);
    match fs::read(&full_path) {
        Ok(bytes) => Ok(Some(ReferenceCapture {
            png_bytes: bytes,
            threshold: sim.threshold,
        })),
        Err(e) => Err(format!(
            "capture_frame similarity: could not read reference {:?}: {e}",
            sim.reference_path,
        )),
    }
}
