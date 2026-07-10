use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aether_substrate::render::VERTEX_BUFFER_BYTES;

use super::capture::CaptureBackend;

/// Boot knobs for `RenderCapability` (ADR-0090). The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `RenderTuningConfigLayer`, the clap-shaped `RenderTuningOverlay`,
/// the `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims — mirrors `AudioConfig`. The chassis
/// main resolves it and threads the value into the hand-built
/// [`RenderConfig`], which also carries non-knob wiring (capture
/// backend, test observability) and so can't ride the derive itself.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_RENDER", cli_prefix = "render")]
pub struct RenderTuningConfig {
    /// `AETHER_RENDER_VERTEX_BUFFER_BYTES=<bytes>` per-frame vertex
    /// buffer cap: the size the GPU vertex buffer is created with and
    /// the byte count the render accumulator truncates to (with a
    /// warn) when a frame's triangles exceed it. Default
    /// [`VERTEX_BUFFER_BYTES`] (64 MiB, ~932k triangles at 72 bytes
    /// each).
    #[config(default = 67_108_864)]
    pub vertex_buffer_bytes: usize,
}

/// Configuration for `RenderCapability`. `vertex_buffer_bytes` is
/// the maximum bytes the render accumulator will hold before
/// truncating with a warn, and the size the GPU vertex buffer is
/// created with — default [`VERTEX_BUFFER_BYTES`]; the chassis main
/// overrides it from the resolved [`RenderTuningConfig`] knob.
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
        Self { vertex_buffer_bytes: VERTEX_BUFFER_BYTES, observed_kinds: None, capture_backend: None, assets_dir: None }
    }
}
