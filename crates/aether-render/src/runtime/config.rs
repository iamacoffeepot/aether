use std::path::PathBuf;
#[cfg(feature = "desktop")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

#[cfg(feature = "desktop")]
use winit::window::Window;

/// The late-bound winit window handle, shared between the desktop chassis's
/// winit `resumed` handler (which fills it exactly once after
/// `create_window`) and the pumped render runtime's state (ADR-0161 slice
/// R2), which reads it in `on_frame` to boot wgpu lazily. Structurally the
/// same one-shot `Arc<OnceLock<Arc<Window>>>` the `aether.window` desktop
/// actor receives — the chassis mints one cell and clones it into both
/// actors' params so they observe the same window (the task's local-alias
/// option: the underlying type is identical to `aether_window`'s
/// `WindowCell`, so a chassis can hand one cell to both without a crate
/// edge). Behind the `desktop` feature: only a desktop chassis pulls winit.
#[cfg(feature = "desktop")]
pub type WindowCell = Arc<OnceLock<Arc<Window>>>;

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
/// short-circuits before any handler runs); pre-PR-E2 the legacy
/// path pushed the raw `kind_name` regardless of dispatch outcome,
/// but tests only use the list as a diagnostic in failure messages
/// so the narrower semantic is fine.
#[derive(Clone, Default)]
pub struct RenderParams {
    pub observed_kinds: Option<Arc<Mutex<Vec<String>>>>,
    /// Resolved path for the `"assets"` namespace, used by the
    /// `capture_frame` handler to read reference images for
    /// similarity checks (iamacoffeepot/aether#1780). The handler
    /// reads the reference PNG synchronously (on the cap dispatcher
    /// thread, not the render thread) and passes the raw bytes
    /// through `PendingCapture.reference`. `None` disables
    /// similarity checks with a descriptive `Err` reply.
    pub assets_dir: Option<PathBuf>,
}

/// Composer-supplied construction params for the pumped render runtime
/// (`PumpedRenderCapability`, ADR-0161 slice R2). A dedicated params
/// channel rather than an extension of the pooled [`RenderParams`]: the
/// `WindowCell` is winit-typed and desktop-only, so a separate struct
/// (mirroring aether-window's `DesktopWindowParams`) keeps the shared
/// pooled params — and its consumers — byte-for-byte untouched.
///
/// ADR-0161 slice R4 lifts the struct off the `desktop` gate (its winit
/// `window` field stays gated inside) so the substrate harness's offscreen
/// pumped runtime can construct it without pulling winit.
#[derive(Clone, Default)]
pub struct PumpedRenderParams {
    /// `SubstrateHarness` observation sink (see [`RenderParams::observed_kinds`]).
    pub observed_kinds: Option<Arc<Mutex<Vec<String>>>>,
    /// Resolved `"assets"` root for `capture_frame` similarity references
    /// (see [`RenderParams::assets_dir`]).
    pub assets_dir: Option<PathBuf>,
    /// The shared late-bound window handle the runtime boots wgpu against
    /// on the first `on_frame` after the chassis's `resumed` fills the
    /// cell. The chassis mints one [`WindowCell`] and clones it into both
    /// this and the `aether.window` desktop actor's params, so both
    /// observe the same window. `None` in tests, which never own a surface.
    #[cfg(feature = "desktop")]
    pub window: Option<WindowCell>,
    /// ADR-0161 slice R4: offscreen boot dimensions for a surfaceless
    /// runtime (the substrate harness). `Some((w, h))` makes the lazy
    /// `on_frame` boot stand up a surfaceless GPU at these dimensions when
    /// no window cell is filled; `None` leaves the runtime windowed (or
    /// never booted, in a no-GPU test).
    pub offscreen_size: Option<(u32, u32)>,
    /// Resolved `AETHER_WIREFRAME` value (argv > env > default), threaded
    /// so the lazy wgpu boot picks the wireframe mode the desktop `Gpu`
    /// boot does. `None` / `"off"` is filled faces.
    pub wireframe: Option<String>,
}
