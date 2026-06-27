use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aether_substrate::render::VERTEX_BUFFER_BYTES;

use super::capture::CaptureBackend;

/// Configuration for `RenderCapability`. `vertex_buffer_bytes` is
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
