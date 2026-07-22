//! Similarity-reference resolution for the `aether.render` cap's
//! `capture_frame` handler (iamacoffeepot/aether#1780). The pumped runtime
//! reads the optional reference PNG synchronously off the render hot path
//! via [`resolve_reference`]; the resolved [`ReferenceCapture`] rides the
//! pending capture until `on_frame` scores it against the readback RGBA.

use std::fs;
use std::path::Path;

use aether_kinds::SimilarityCheck;
use aether_substrate::capture::ReferenceCapture;

/// Resolve the optional reference image for a `#1780` similarity
/// check, reading it synchronously on the cap dispatcher thread so all
/// filesystem I/O stays off the render hot path. `Ok(None)` when no
/// check was requested; `Err(message)` when the reference can't be
/// used (unsupported namespace, no assets dir, forbidden path, or an
/// unreadable file) — the caller replies that message as
/// `CaptureFrameResult::Err`.
pub fn resolve_reference(
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
        return Err("capture_frame similarity: no assets directory is configured on this \
                    chassis; similarity checks are unavailable"
            .to_owned());
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
        Ok(bytes) => Ok(Some(ReferenceCapture { png_bytes: bytes, threshold: sim.threshold })),
        Err(e) => Err(format!("capture_frame similarity: could not read reference {:?}: {e}", sim.reference_path)),
    }
}
