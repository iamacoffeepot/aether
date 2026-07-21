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
//! Chassis-wide knobs (`workers`, `boot_manifest`, `rpc_port`), the lifecycle
//! cap's `advance_timeout_millis`, and per-chassis knobs (`window_mode` /
//! `window_title` for desktop, `tick_hz` for headless) are now fully migrated
//! to `#[derive(aether_substrate::Config)]` overlays: `ChassisBootOverlay` /
//! `LifecycleOverlay` / `WindowOverlay` / `TickOverlay`. Only `rpc_port`
//! remains hand-written (its per-chassis default differs).
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
// next to the domain struct in each cap's own crate. Re-exporting them
// here keeps the `cli.common.<cap>.into_layer()` call sites unchanged.
// The `NamespaceRoots` overlay's name follows the domain struct
// (`NamespaceRootsOverlay`), not the namespace prefix (`FsOverlay`) —
// alias the historical name so the bundle's compose code keeps
// reading.
pub use aether_anthropic::AnthropicOverlay;
pub use aether_audio::AudioOverlay;
pub use aether_contentgen::ContentGenOverlay;
pub use aether_engine::EngineOverlay;
pub use aether_fs::NamespaceRootsOverlay as FsOverlay;
pub use aether_gemini::GeminiOverlay;
pub use aether_http::HttpOverlay;
pub use aether_http::HttpServerOverlay;
pub use aether_lifecycle::LifecycleOverlay;

pub use crate::boot::ChassisBootOverlay;
pub use crate::tick::TickOverlay;
pub use crate::window::WindowOverlay;

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
    /// Content-gen staging root: `--gen-dir` / `AETHER_GEN_DIR`.
    #[command(flatten)]
    pub contentgen: ContentGenOverlay,
    /// Shared chassis boot knobs: `--workers`, `--boot-manifest`.
    #[command(flatten)]
    pub chassis_boot: ChassisBootOverlay,
    /// Lifecycle cap knob: `--lifecycle-advance-timeout-millis` (ADR-0156 §3
    /// relocated it off `ChassisBootConfig` onto the lifecycle cap's config).
    #[command(flatten)]
    pub lifecycle: LifecycleOverlay,

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

    /// Sectioned TOML chassis config file. Values from this file sit
    /// below env and argv in the source stack.
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<String>,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "print-config")]
    pub print_config: bool,

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

    /// Sectioned TOML chassis config file. Values from this file sit
    /// below env and argv in the source stack.
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<String>,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "print-config")]
    pub print_config: bool,

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

    /// Sectioned TOML chassis config file. Values from this file sit
    /// below env and argv in the source stack.
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<String>,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "print-config")]
    pub print_config: bool,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps,
    /// build provenance) as JSON and exit before boot (ADR-0115, issue
    /// 1953). The hub's binary store forks `<binary> --describe` once at
    /// upload time to capture what a stored binary is.
    #[arg(long = "describe")]
    pub describe: bool,
}

#[cfg(test)]
mod checkability_tests {
    //! ADR-0156 §5: the CLI roots stay hand-written static clap structs, so
    //! each carries a checkable invariant in place of the old lockstep comment
    //! — its long-flag set equals the union of the flags declared by the
    //! overlays of the members it composes (plus the meta flags that select the
    //! source itself: `--config` / `--print-config` / `--describe`). A cap added
    //! to a chassis's composition without flattening its overlay into the root
    //! (or a stale flag left in the root) fails the assertion honestly.

    use super::{CommonOverlay, DesktopCli, EngineOverlay, HeadlessCli, HubCli, TickOverlay};
    use crate::window::WindowOverlay;
    use aether_audio::AudioOverlay;
    use clap::{Args, CommandFactory};
    use std::collections::BTreeSet;

    /// Every `--long` flag a clap command declares.
    fn long_flags(command: &clap::Command) -> BTreeSet<String> {
        command.get_arguments().filter_map(|arg| arg.get_long().map(str::to_owned)).collect()
    }

    /// The long flags an [`Args`] overlay contributes, gathered by augmenting a
    /// throwaway command with it — the same flags the chassis root gets by
    /// `#[command(flatten)]`-ing the overlay.
    fn overlay_flags<T: Args>() -> BTreeSet<String> {
        long_flags(&T::augment_args(clap::Command::new("probe")))
    }

    /// The source-selecting meta flags every chassis root carries directly
    /// (they name the file source and the print/describe exits, so they belong
    /// to no cap member).
    fn meta_flags() -> BTreeSet<String> {
        ["config", "print-config", "describe"].into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn desktop_root_flags_equal_composed_overlay_set() {
        let mut expected = overlay_flags::<CommonOverlay>();
        expected.extend(overlay_flags::<AudioOverlay>());
        expected.extend(overlay_flags::<WindowOverlay>());
        expected.extend(meta_flags());
        assert_eq!(long_flags(&DesktopCli::command()), expected);
    }

    #[test]
    fn headless_root_flags_equal_composed_overlay_set() {
        let mut expected = overlay_flags::<CommonOverlay>();
        expected.extend(overlay_flags::<TickOverlay>());
        expected.extend(meta_flags());
        assert_eq!(long_flags(&HeadlessCli::command()), expected);
    }

    #[test]
    fn hub_root_flags_equal_composed_overlay_set() {
        // The hub composes only the engines cap; `--rpc-port` is a direct root
        // flag (its per-chassis default differs) alongside the meta flags.
        let mut expected = overlay_flags::<EngineOverlay>();
        expected.insert("rpc-port".to_owned());
        expected.extend(meta_flags());
        assert_eq!(long_flags(&HubCli::command()), expected);
    }
}
