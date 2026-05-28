//! Per-chassis clap CLI roots and per-cap argv overlays (ADR-0090 unit
//! d, issue 1258). Each chassis bin calls `<Cli>::parse()` and threads
//! the resolved overlays through `*Env::from_env_with_argv(cli)`; each
//! overlay's `into_layer()` writes argv-set fields into a partial
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

use std::collections::HashSet;
use std::path::PathBuf;

use aether_capabilities::anthropic::AnthropicConfigLayer;
use aether_capabilities::audio::AudioConfigLayer;
use aether_capabilities::fs::NamespaceRootsLayer;
use aether_capabilities::gemini::GeminiConfigLayer;
use aether_capabilities::http::HttpConfigLayer;
use aether_substrate::handle_store::PersistConfigLayer;
use clap::{Args, Parser};
use confique::{Config, Layer};

/// Argv overlay for the `aether.http` cap. One `Option<T>` per
/// `HttpConfigLayer` field; [`Self::into_layer`] maps unset → partial
/// `None`, set → partial `Some(v)`.
#[derive(Args, Debug, Default, Clone)]
pub struct HttpOverlay {
    /// `AETHER_HTTP_DISABLE` — swap in the disabled adapter.
    #[arg(id = "http_disable", long = "http-disable", num_args = 0..=1, default_missing_value = "true")]
    pub disable: Option<bool>,
    /// `AETHER_HTTP_ALLOWLIST` — comma-separated hostnames.
    #[arg(id = "http_allowlist", long = "http-allowlist")]
    pub allowlist: Option<String>,
    /// `AETHER_HTTP_REQUIRE_HTTPS` — reject `http://` URLs.
    #[arg(id = "http_require_https", long = "http-require-https", num_args = 0..=1, default_missing_value = "true")]
    pub require_https: Option<bool>,
    /// `AETHER_HTTP_MAX_BODY_BYTES` — response body cap.
    #[arg(id = "http_max_body_bytes", long = "http-max-body-bytes")]
    pub max_body_bytes: Option<usize>,
    /// `AETHER_HTTP_TIMEOUT_MS` — default per-request timeout.
    #[arg(id = "http_timeout_ms", long = "http-timeout-ms")]
    pub timeout_ms: Option<u32>,
}

impl HttpOverlay {
    /// Write argv-set fields into a fresh partial layer. `None` →
    /// partial `None` (env / default takes over); `Some(v)` → partial
    /// `Some(v)`.
    #[must_use]
    pub fn into_layer(self) -> <HttpConfigLayer as Config>::Layer {
        let Self {
            disable,
            allowlist,
            require_https,
            max_body_bytes,
            timeout_ms,
        } = self;
        let mut layer = <HttpConfigLayer as Config>::Layer::empty();
        if let Some(v) = disable {
            layer.disabled = Some(v);
        }
        if let Some(s) = allowlist {
            // Same total parser the env side uses (CSV → HashSet, trim,
            // drop empties). Mirrored inline to avoid making the
            // parser pub on the cap.
            let set: HashSet<String> = s
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
                .collect();
            layer.allowlist = Some(set);
        }
        if let Some(v) = require_https {
            layer.require_https = Some(v);
        }
        if let Some(v) = max_body_bytes {
            layer.max_body_bytes = Some(v);
        }
        if let Some(v) = timeout_ms {
            layer.timeout_ms = Some(v);
        }
        layer
    }
}

/// Argv overlay for the `aether.audio` cap (desktop only). Headless
/// runs without an audio device, so [`DesktopCli`] is the only chassis
/// that flatten-includes this.
#[derive(Args, Debug, Default, Clone)]
pub struct AudioOverlay {
    /// `AETHER_AUDIO_DISABLE` — skip cpal init entirely.
    #[arg(id = "audio_disable", long = "audio-disable", num_args = 0..=1, default_missing_value = "true")]
    pub disable: Option<bool>,
    /// `AETHER_AUDIO_SAMPLE_RATE` — requested sample rate in Hz.
    #[arg(id = "audio_sample_rate", long = "audio-sample-rate")]
    pub sample_rate: Option<u32>,
}

impl AudioOverlay {
    #[must_use]
    pub fn into_layer(self) -> <AudioConfigLayer as Config>::Layer {
        let Self {
            disable,
            sample_rate,
        } = self;
        let mut layer = <AudioConfigLayer as Config>::Layer::empty();
        if let Some(v) = disable {
            layer.disabled = Some(v);
        }
        if let Some(v) = sample_rate {
            // The layer holds the raw string so the cap can apply the
            // soft `.parse::<u32>().ok()` (an unparseable env value →
            // `None`). On the argv path the value is already typed.
            layer.requested_sample_rate = Some(v.to_string());
        }
        layer
    }
}

/// Argv overlay for the `aether.fs` cap namespace roots.
#[derive(Args, Debug, Default, Clone)]
pub struct FsOverlay {
    /// `AETHER_SAVE_DIR` — writable per-user persistent root.
    #[arg(id = "save_dir", long = "save-dir")]
    pub save_dir: Option<PathBuf>,
    /// `AETHER_ASSETS_DIR` — read-only assets root.
    #[arg(id = "assets_dir", long = "assets-dir")]
    pub assets_dir: Option<PathBuf>,
    /// `AETHER_CONFIG_DIR` — writable per-user config root.
    #[arg(id = "config_dir", long = "config-dir")]
    pub config_dir: Option<PathBuf>,
}

impl FsOverlay {
    #[must_use]
    pub fn into_layer(self) -> <NamespaceRootsLayer as Config>::Layer {
        let Self {
            save_dir,
            assets_dir,
            config_dir,
        } = self;
        let mut layer = <NamespaceRootsLayer as Config>::Layer::empty();
        if let Some(p) = save_dir {
            layer.save = Some(p);
        }
        if let Some(p) = assets_dir {
            layer.assets = Some(p);
        }
        if let Some(p) = config_dir {
            layer.config = Some(p);
        }
        layer
    }
}

/// Argv overlay for the `aether.anthropic` cap (ADR-0050).
#[derive(Args, Debug, Default, Clone)]
pub struct AnthropicOverlay {
    /// `ANTHROPIC_API_KEY` — Messages-API key. Empty → disabled
    /// adapter.
    #[arg(id = "anthropic_api_key", long = "anthropic-api-key")]
    pub api_key: Option<String>,
    /// `AETHER_ANTHROPIC_DISABLE` — force the disabled adapter.
    #[arg(id = "anthropic_disable", long = "anthropic-disable", num_args = 0..=1, default_missing_value = "true")]
    pub disable: Option<bool>,
    /// `AETHER_ANTHROPIC_MAX_IN_FLIGHT` — per-cap concurrency bound.
    #[arg(id = "anthropic_max_in_flight", long = "anthropic-max-in-flight")]
    pub max_in_flight: Option<usize>,
    /// `AETHER_ANTHROPIC_TIMEOUT_MS` — per-request timeout.
    #[arg(id = "anthropic_timeout_ms", long = "anthropic-timeout-ms")]
    pub timeout_ms: Option<u32>,
}

impl AnthropicOverlay {
    #[must_use]
    pub fn into_layer(self) -> <AnthropicConfigLayer as Config>::Layer {
        let Self {
            api_key,
            disable,
            max_in_flight,
            timeout_ms,
        } = self;
        let mut layer = <AnthropicConfigLayer as Config>::Layer::empty();
        if let Some(v) = api_key {
            layer.api_key = Some(v);
        }
        if let Some(v) = disable {
            layer.disabled = Some(v);
        }
        if let Some(v) = max_in_flight {
            layer.max_in_flight = Some(v);
        }
        if let Some(v) = timeout_ms {
            layer.timeout_ms = Some(v);
        }
        layer
    }
}

/// Argv overlay for the `aether.gemini` cap (ADR-0050).
#[derive(Args, Debug, Default, Clone)]
pub struct GeminiOverlay {
    /// `GEMINI_API_KEY` — Google API key. Empty → disabled adapter.
    #[arg(id = "gemini_api_key", long = "gemini-api-key")]
    pub api_key: Option<String>,
    /// `AETHER_GEMINI_DISABLE` — force the disabled adapter.
    #[arg(id = "gemini_disable", long = "gemini-disable", num_args = 0..=1, default_missing_value = "true")]
    pub disable: Option<bool>,
    /// `AETHER_GEMINI_MAX_IN_FLIGHT` — per-cap concurrency bound.
    #[arg(id = "gemini_max_in_flight", long = "gemini-max-in-flight")]
    pub max_in_flight: Option<usize>,
    /// `AETHER_GEMINI_TIMEOUT_MS` — per-request timeout.
    #[arg(id = "gemini_timeout_ms", long = "gemini-timeout-ms")]
    pub timeout_ms: Option<u32>,
}

impl GeminiOverlay {
    #[must_use]
    pub fn into_layer(self) -> <GeminiConfigLayer as Config>::Layer {
        let Self {
            api_key,
            disable,
            max_in_flight,
            timeout_ms,
        } = self;
        let mut layer = <GeminiConfigLayer as Config>::Layer::empty();
        if let Some(v) = api_key {
            layer.api_key = Some(v);
        }
        if let Some(v) = disable {
            layer.disabled = Some(v);
        }
        if let Some(v) = max_in_flight {
            layer.max_in_flight = Some(v);
        }
        if let Some(v) = timeout_ms {
            layer.timeout_ms = Some(v);
        }
        layer
    }
}

/// Argv overlay for the ADR-0049 handle-store persistence knobs. The
/// `dir` / `persist_disable` flags shadow `AETHER_HANDLE_STORE_DIR` /
/// `AETHER_HANDLE_STORE_PERSIST_DISABLE` directly; `disk_budget_bytes`
/// / `eviction_tick_secs` ride [`PersistConfigLayer`] and merge with
/// env via the confique builder. `max_bytes` is the in-memory soft
/// budget (`AETHER_HANDLE_STORE_MAX_BYTES`), structurally separate
/// from the on-disk persist config.
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
}
