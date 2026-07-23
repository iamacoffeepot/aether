use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Boot knobs for `RenderCapability` (ADR-0090). The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `RenderTuningConfigLayer`, the clap-shaped `RenderTuningOverlay`,
/// the `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims — mirrors `AudioConfig`. This is the cap's
/// operator-resolvable `Config` (ADR-0156 §3); the non-knob wiring
/// (chassis-derived assets root, test observability) rides the separate
/// [`RenderParams`] channel instead.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_RENDER", cli_prefix = "render")]
pub struct RenderTuningConfig {
    /// Per-frame vertex buffer size in bytes; frames beyond it are truncated.
    ///
    /// The size the GPU vertex buffer is created with and the byte count
    /// the render accumulator truncates to (with a warn) when a frame's
    /// triangles exceed it. Default
    /// [`VERTEX_BUFFER_BYTES`](aether_substrate::render::VERTEX_BUFFER_BYTES)
    /// (64 MiB, ~932k triangles at 72 bytes each).
    #[config(default = 67_108_864)]
    pub vertex_buffer_bytes: usize,
}

/// Composer-supplied construction params for `RenderCapability`
/// (ADR-0156 §3): the non-knob wiring the chassis computes at boot, kept
/// off the operator-resolvable [`RenderTuningConfig`] `Config`.
///
/// `observed_kinds`, when set, has every successfully-dispatched
/// inbound mail's kind name pushed to it from the cap's `#[handler]`
/// methods — used by the in-process substrate-harness to assert what kinds
/// the cap has seen. Production chassis leave it `None` (zero
/// overhead). Decode failures and unknown kinds don't push (the
/// macro miss path warn-logs at the chassis-side dispatcher and
/// short-circuits before any handler runs).
#[derive(Clone, Default)]
pub struct RenderParams {
    /// `SubstrateHarness` observation sink.
    pub observed_kinds: Option<Arc<Mutex<Vec<String>>>>,
    /// Resolved path for the `"assets"` namespace, used by the
    /// `capture_frame` handler to read reference images for similarity
    /// checks (iamacoffeepot/aether#1780). The handler resolves the
    /// reference PNG synchronously and passes the raw bytes through the
    /// pending capture. `None` disables similarity checks with a
    /// descriptive `Err` reply.
    pub assets_dir: Option<PathBuf>,
    /// ADR-0161 slice R4: offscreen boot dimensions for a surfaceless
    /// runtime (the substrate harness). `Some((w, h))` makes the lazy
    /// `on_frame` boot stand up a surfaceless GPU at these dimensions when
    /// no window target is requested; `None` leaves the runtime windowed
    /// (or never booted, in a no-GPU test).
    pub offscreen_size: Option<(u32, u32)>,
    /// Resolved `AETHER_WIREFRAME` value (argv > env > default), threaded
    /// so the lazy wgpu boot picks the wireframe mode. `None` / `"off"` is
    /// filled faces.
    pub wireframe: Option<String>,
}
