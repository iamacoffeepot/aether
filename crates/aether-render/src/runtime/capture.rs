//! Similarity-reference resolution for the `aether.render` cap's
//! `capture_frame` handler (iamacoffeepot/aether#1780). The pumped runtime
//! reads the optional reference PNG synchronously off the render hot path
//! via [`resolve_reference`]; the resolved [`ReferenceCapture`] rides the
//! pending capture until `on_frame` scores it against the readback RGBA.
//!
//! [`PendingCapture`] is that parked capture: the retained reply guard plus
//! everything the readback still needs. It lives here rather than beside the
//! frame loop because every field on it is capture state, and the runtime only
//! ever asks it two questions — is it ready, and has it expired.

use std::fs;
use std::path::Path;

use std::time::Instant;

use aether_kinds::{FrameCheck, SimilarityCheck, WindowId};
use aether_substrate::capture::ReferenceCapture;
use aether_substrate::chassis::inbox::InboundMail;
use aether_substrate::mail::Mail;

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

/// A parked capture, as plain owned state (ADR-0161 §Decision 4) — no
/// `Arc`, no atomic, no cross-thread queue. The retained [`InboundMail`]
/// guard defers the reply a frame (or more) past `on_capture_frame`; its
/// un-fired `record_finished` keeps the inbound's chain open until the
/// reply lands (ADR-0080 §6, ADR-0106).
pub struct PendingCapture {
    /// Selected desktop target. `None` is the explicit surfaceless path.
    pub window: Option<WindowId>,
    pub reply: InboundMail,
    pub after_mails: Vec<Mail>,
    /// `FrameCheck` verdict requests, scored on the read-back RGBA in
    /// `on_frame`'s ready-branch (ADR-0161 §Decision 4). The scorer lives in
    /// `aether_substrate::render::visual`, so the branch is reachable without
    /// a downstream cycle.
    pub checks: Vec<FrameCheck>,
    /// Optional similarity reference (issue 1780), scored alongside `checks`.
    pub reference: Option<ReferenceCapture>,
    /// Count of pre-mail settlements still awaited; `on_pre_settled`
    /// decrements it, and `on_frame` captures once it reaches zero.
    pub pre_remaining: usize,
    /// Wall-clock instant past which the capture wedges to `Err`.
    pub deadline: Instant,
}

impl PendingCapture {
    /// Ready to read back — every pre-mail chain has settled.
    pub fn is_ready(&self) -> bool {
        self.pre_remaining == 0
    }

    /// Past its wedge deadline (`FRAME_SETTLEMENT_CAP` since parking).
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}
