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
//! `--http-timeout-ms`, `GEMINI_API_KEY` → `--gemini-api-key`,
//! `AETHER_HANDLE_STORE_DISK_BUDGET_BYTES` → `--handle-store-disk-budget-bytes`.
//! Bool flags accept zero or one value (`--http-disable` ⇒ `true`,
//! `--http-disable=false` ⇒ `false`, absent ⇒ `None`), matching the
//! env-side `parse_flag` semantics.
//!
//! Chassis-wide knobs (`workers`, `tick_hz`, `window_mode`,
//! `window_title`, `rpc_port`) live as plain `Option<T>` fields on the
//! root `CommonOverlay` / `DesktopCli` / `HeadlessCli` and are
//! ad-hoc-shadowed in the bin (`cli.workers.or_else(parse_workers_env)`).
//! Unit e1 will lift them into their own confique layers.
//!
//! ADR-0090 unit g (iamacoffeepot/aether#1264): the per-cap `*Overlay`
//! structs now ride the `#[derive(aether_substrate::Config)]` next to
//! the domain struct in the cap crate. This file re-exports them so
//! `cli.common.http.into_layer()` call sites stay unchanged; the
//! `PersistOverlay` + chassis-root CLI structs stay hand-written
//! because they cover chassis-shape (cross-cap) composition the
//! derive deliberately doesn't try to model.

use std::path::PathBuf;

use aether_substrate::handle_store::PersistConfigLayer;
use clap::{Args, Parser};
use confique::{Config, Layer};

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
pub use aether_capabilities::http_server::HttpServerOverlay;

/// Argv overlay for the ADR-0049 handle-store persistence knobs. The
/// `dir` / `persist_disable` flags shadow `AETHER_HANDLE_STORE_DIR` /
/// `AETHER_HANDLE_STORE_PERSIST_DISABLE` directly; `disk_budget_bytes`
/// / `eviction_tick_secs` ride [`PersistConfigLayer`] and merge with
/// env via the confique builder. `max_bytes` is the in-memory soft
/// budget (`AETHER_HANDLE_STORE_MAX_BYTES`), structurally separate
/// from the on-disk persist config.
///
/// Stays hand-written (not derive-emitted) because `PersistConfig`'s
/// `from_argv_then_env` takes four arguments (chassis vote + dir
/// lookup + env short-circuit + numeric layer) that don't fit the
/// derive's `Layer → Self` shape. The `aether-substrate::config`
/// module docs hold the rationale verbatim.
#[derive(Args, Debug, Default, Clone)]
pub struct PersistOverlay {
    /// `AETHER_HANDLE_STORE_DIR` — on-disk persistence root.
    #[arg(id = "handle_store_dir", long = "handle-store-dir")]
    pub dir: Option<PathBuf>,
    /// `AETHER_HANDLE_STORE_PERSIST_DISABLE` — skip on-disk
    /// persistence entirely (chassis runs in-memory only).
    #[arg(id = "handle_store_persist_disable", long = "handle-store-persist-disable", num_args = 0..=1, default_missing_value = "true")]
    pub persist_disable: Option<bool>,
    /// `AETHER_HANDLE_STORE_MAX_BYTES` — in-memory soft byte budget.
    #[arg(id = "handle_store_max_bytes", long = "handle-store-max-bytes")]
    pub max_bytes: Option<usize>,
    /// `AETHER_HANDLE_STORE_DISK_BUDGET_BYTES` — on-disk byte budget.
    #[arg(
        id = "handle_store_disk_budget_bytes",
        long = "handle-store-disk-budget-bytes"
    )]
    pub disk_budget_bytes: Option<u64>,
    /// `AETHER_HANDLE_STORE_DISK_EVICTION_TICK_SECS` — eviction tick
    /// interval.
    #[arg(
        id = "handle_store_disk_eviction_tick_secs",
        long = "handle-store-disk-eviction-tick-secs"
    )]
    pub eviction_tick_secs: Option<u64>,
}

impl PersistOverlay {
    /// Map the numeric overlay knobs into a partial layer. The
    /// `dir` / `persist_disable` / `max_bytes` axes are structural —
    /// they live outside `PersistConfigLayer` and ride the bin's
    /// composition instead, so [`Self::numeric_layer`] takes `&self`
    /// rather than consuming the overlay.
    #[must_use]
    pub fn numeric_layer(&self) -> <PersistConfigLayer as Config>::Layer {
        let mut layer = <PersistConfigLayer as Config>::Layer::empty();
        if let Some(v) = self.disk_budget_bytes {
            layer.disk_budget_bytes = Some(v);
        }
        if let Some(v) = self.eviction_tick_secs {
            layer.eviction_tick_secs = Some(v);
        }
        layer
    }
}

/// Argv overlay shared by every full-stack chassis (desktop +
/// headless). Captures every cap whose config layer is the same on
/// both chassis. Per-chassis extras (audio for desktop, tick-hz +
/// window-* for desktop) live on their own root struct.
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
    #[command(flatten)]
    pub persist: PersistOverlay,

    /// `AETHER_WORKERS` — worker pool size override.
    #[arg(long)]
    pub workers: Option<usize>,

    /// `AETHER_RPC_PORT` — `aether.rpc.server` bind port. Absent →
    /// chassis-specific default (desktop / headless skip the RPC
    /// server entirely; hub falls back to `DEFAULT_RPC_PORT`).
    #[arg(long = "rpc-port")]
    pub rpc_port: Option<u16>,

    /// `AETHER_BOOT_MANIFEST` — path to a `BundleManifest` JSON of
    /// components to auto-load at boot (the runtime twin of the
    /// standalone-bundle compile-time pack). Absent → boot componentless.
    #[arg(long = "boot-manifest")]
    pub boot_manifest: Option<String>,
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

    /// `AETHER_WINDOW_MODE` — `windowed[:WxH]` /
    /// `fullscreen-borderless` / `exclusive:WxH@HZ`.
    #[arg(long = "window-mode")]
    pub window_mode: Option<String>,
    /// `AETHER_WINDOW_TITLE` — window title text.
    #[arg(long = "window-title")]
    pub window_title: Option<String>,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "config")]
    pub config: bool,
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

    /// `AETHER_TICK_HZ` — tick cadence in hertz (default 60).
    #[arg(long = "tick-hz")]
    pub tick_hz: Option<u32>,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "config")]
    pub config: bool,
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
}
