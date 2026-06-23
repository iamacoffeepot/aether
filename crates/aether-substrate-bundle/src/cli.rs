//! Per-chassis clap CLI roots (ADR-0090 unit d, issue 1258). Each
//! chassis bin calls `<Cli>::parse()` and threads the resolved
//! overlays through `*Env::from_env_with_argv(cli)`; each overlay's
//! `into_layer()` writes argv-set fields into a partial
//! `<*ConfigLayer as confique::Config>::Layer`, which the cap's
//! `from_argv_then_env(...)` then preloads ahead of `.env()` so argv
//! beats env beats literal defaults. Absent flags resolve `None` and
//! fall through to env-only resolution — boot is byte-identical when
//! argv is empty.
//!
//! Flag naming is mechanical: strip an `AETHER_` (or top-level)
//! prefix, lowercase, hyphenate. `AETHER_HTTP_TIMEOUT_MS` →
//! `--http-timeout-ms`, `GEMINI_API_KEY` → `--gemini-api-key`.
//! Bool flags accept zero or one value (`--http-disable` ⇒ `true`,
//! `--http-disable=false` ⇒ `false`, absent ⇒ `None`), matching
//! confique's native env-side bool deserialization.
//!
//! Chassis-wide knobs (`workers`, `boot_manifest`,
//! `lifecycle_advance_timeout_millis`, `rpc_port`) and per-chassis knobs
//! (`window_mode` / `window_title` for desktop, `tick_hz` for headless)
//! are now fully migrated to `#[derive(aether_substrate::Config)]` overlays:
//! `ChassisBootOverlay` / `WindowOverlay` / `TickOverlay`. Only
//! `rpc_port` remains hand-written (its per-chassis default differs).
//!
//! ADR-0090 unit g (iamacoffeepot/aether#1264): the per-cap `*Overlay`
//! structs now ride the `#[derive(aether_substrate::Config)]` next to
//! the domain struct in the cap crate. This file re-exports them so
//! `cli.common.http.into_layer()` call sites stay unchanged; the
//! chassis-root CLI structs stay hand-written because they cover
//! chassis-shape (cross-cap) composition the derive deliberately
//! doesn't try to model.

use clap::{Args, Parser};

// Per-cap overlays are emitted by `#[derive(aether_substrate::Config)]`
// next to the domain struct in `aether-capabilities`. Re-exporting them
// here keeps the `cli.common.<cap>.into_layer()` call sites unchanged.
// The `NamespaceRoots` overlay's name follows the domain struct
// (`NamespaceRootsOverlay`), not the namespace prefix (`FsOverlay`) —
// alias the historical name so the bundle's compose code keeps
// reading.
pub use aether_capabilities::EngineOverlay;
pub use aether_capabilities::anthropic::AnthropicOverlay;
pub use aether_capabilities::audio::AudioOverlay;
pub use aether_capabilities::fs::NamespaceRootsOverlay as FsOverlay;
pub use aether_capabilities::gemini::GeminiOverlay;
pub use aether_capabilities::http::HttpOverlay;
pub use aether_capabilities::http::HttpServerOverlay;

pub use crate::chassis_common::ChassisBootOverlay;
pub use crate::desktop::driver::WindowOverlay;
pub use crate::headless::driver::TickOverlay;

/// Argv overlay shared by every full-stack chassis (desktop +
/// headless). Captures every cap whose config layer is the same on
/// both chassis. Per-chassis extras (audio for desktop, tick / window
/// for desktop) live on their own root struct.
#[derive(Args, Debug, Default, Clone)]
pub struct CommonOverlay {
    #[command(flatten)]
    pub http: HttpOverlay,
    #[command(flatten)]
    pub http_server: HttpServerOverlay,
    #[command(flatten)]
    pub fs: FsOverlay,
    #[command(flatten)]
    pub anthropic: AnthropicOverlay,
    #[command(flatten)]
    pub gemini: GeminiOverlay,
    /// Shared chassis boot knobs: `--workers`, `--boot-manifest`,
    /// `--lifecycle-advance-timeout-millis`.
    #[command(flatten)]
    pub chassis_boot: ChassisBootOverlay,

    /// `AETHER_RPC_PORT` — `aether.rpc.server` bind port. Absent →
    /// chassis-specific default (desktop / headless skip the RPC
    /// server entirely; hub falls back to `DEFAULT_RPC_PORT`).
    #[arg(long = "rpc-port")]
    pub rpc_port: Option<u16>,
}

/// Desktop chassis CLI root.
#[derive(Parser, Debug, Default, Clone)]
#[command(
    name = "aether-substrate",
    about = "Desktop chassis — winit window + wgpu render + cpal audio. ADR-0035 / ADR-0090."
)]
pub struct DesktopCli {
    #[command(flatten)]
    pub common: CommonOverlay,
    #[command(flatten)]
    pub audio: AudioOverlay,
    /// Desktop window knobs: `--window-mode`, `--window-title`.
    #[command(flatten)]
    pub window: WindowOverlay,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "config")]
    pub config: bool,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps,
    /// build provenance) as JSON and exit before boot (ADR-0115, issue
    /// 1953). The hub's binary store forks `<binary> --describe` once at
    /// upload time to capture what a stored binary is.
    #[arg(long = "describe")]
    pub describe: bool,
}

/// Headless chassis CLI root.
#[derive(Parser, Debug, Default, Clone)]
#[command(
    name = "aether-substrate-headless",
    about = "Headless chassis — std-timer tick driver, nop render. ADR-0035 / ADR-0090."
)]
pub struct HeadlessCli {
    #[command(flatten)]
    pub common: CommonOverlay,
    /// Headless tick knob: `--tick-hz`.
    #[command(flatten)]
    pub tick: TickOverlay,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "config")]
    pub config: bool,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps,
    /// build provenance) as JSON and exit before boot (ADR-0115, issue
    /// 1953). The hub's binary store forks `<binary> --describe` once at
    /// upload time to capture what a stored binary is.
    #[arg(long = "describe")]
    pub describe: bool,
}

/// Hub chassis CLI root — coordinator-only, no full-stack caps.
#[derive(Parser, Debug, Default, Clone)]
#[command(
    name = "aether-substrate-hub",
    about = "Hub chassis — coordinator between aether-mcp + substrate fleet. ADR-0073."
)]
pub struct HubCli {
    /// `AETHER_RPC_PORT` — `aether.rpc.server` bind port (default
    /// 8901).
    #[arg(long = "rpc-port")]
    pub rpc_port: Option<u16>,

    /// Engines-cap knobs — the liveness-heartbeat tuning
    /// (`--hub-heartbeat-interval-secs` / `--hub-heartbeat-miss-limit`,
    /// issue 1339). Flattened from the derive-emitted overlay.
    #[command(flatten)]
    pub engine: EngineOverlay,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "config")]
    pub config: bool,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps,
    /// build provenance) as JSON and exit before boot (ADR-0115, issue
    /// 1953). The hub's binary store forks `<binary> --describe` once at
    /// upload time to capture what a stored binary is.
    #[arg(long = "describe")]
    pub describe: bool,
}
