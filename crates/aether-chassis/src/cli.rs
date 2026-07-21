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
//! ADR-0156 §5 (issue 3872): staging each overlay onto the source stack is
//! **derived**, not hand-maintained. `#[derive(aether_substrate::Config)]` emits
//! a leaf `StageArgv` on every `*Overlay`, and each root here carries
//! `#[derive(aether_substrate::StageArgv)]` — the container half that delegates
//! to every field's `stage`. A chassis then stages its whole CLI in one
//! `cli.stage(&mut sources)` call, so adding an overlay field to a root IS
//! staging it. Non-overlay meta fields (`config` / `print_config` / `describe`)
//! carry `#[stage(skip)]`; an unannotated non-overlay field fails to compile,
//! and a staged-but-never-composed overlay fails boot loudly
//! (`ConfigSources::validate_no_orphan_argv`).
//!
//! Flag naming is mechanical: strip an `AETHER_` (or top-level)
//! prefix, lowercase, hyphenate. `AETHER_HTTP_TIMEOUT_MS` →
//! `--http-timeout-ms`, `GEMINI_API_KEY` → `--gemini-api-key`.
//! Bool flags accept zero or one value (`--http-disable` ⇒ `true`,
//! `--http-disable=false` ⇒ `false`, absent ⇒ `None`), matching
//! confique's native env-side bool deserialization.
//!
//! Chassis-wide knobs (`workers`, `boot_manifest`), the lifecycle cap's
//! `advance_timeout_millis`, the RPC bind port (`rpc_port`), and per-chassis
//! knobs (`window_mode` / `window_title` for desktop, `tick_hz` for headless)
//! are all `#[derive(aether_substrate::Config)]` overlays now:
//! `ChassisBootOverlay` / `LifecycleOverlay` / `RpcServerOverlay` /
//! `WindowOverlay` / `TickOverlay`. `--rpc-port` rides the derive like the rest
//! (#3849 retired its hand-written flag); the per-chassis default lives at each
//! compose site, not in the flag.
//!
//! Issue 3882 closed the remaining `--help` gap: the four tuning overlays every
//! chassis resolved env/file-only are now flattened where the chassis resolves
//! them — `ActorRingOverlay` / `SchedulerTuningOverlay` / `SettlementOverlay`
//! into the shared `CommonOverlay` (both full-stack chassis) and `HubCli` (which
//! hosts its own registry actors), plus `RenderTuningOverlay` into `DesktopCli`
//! alone (headless composes the nop render cap, which resolves no
//! `RenderTuningConfig`). The knobs that legitimately stay env-only — the
//! `RuntimeConfig` log/panic-hook directives (the panic hook reads env directly,
//! below the config layer) and the `FrameSizeConfig` wire cap — are rendered into
//! each root's `after_help` from the same registry `--print-config` walks
//! (`crate::boot::env_only_after_help` / `hub_env_only_after_help`), so help
//! cannot drift from the registry.
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
pub use aether_harness_substrate::SettlementOverlay;
pub use aether_http::HttpOverlay;
pub use aether_http::HttpServerOverlay;
pub use aether_lifecycle::LifecycleOverlay;
pub use aether_render::RenderTuningOverlay;
pub use aether_rpc::RpcServerOverlay;

pub use crate::boot::{
    ActorRingOverlay, ChassisBootOverlay, SchedulerTuningOverlay, env_only_after_help, hub_env_only_after_help,
};
pub use crate::tick::TickOverlay;
pub use crate::window::WindowOverlay;

/// Argv overlay shared by every full-stack chassis (desktop +
/// headless). Captures every cap whose config layer is the same on
/// both chassis. Per-chassis extras (audio for desktop, tick / window
/// for desktop) live on their own root struct.
#[derive(Args, Debug, Default, Clone, aether_substrate::StageArgv)]
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
    pub generated_asset_staging: ContentGenOverlay,
    /// Shared chassis boot knobs: `--workers`, `--boot-manifest`.
    #[command(flatten)]
    pub chassis_boot: ChassisBootOverlay,
    /// Lifecycle cap knob: `--lifecycle-advance-timeout-millis` (ADR-0156 §3
    /// relocated it off `ChassisBootConfig` onto the lifecycle cap's config).
    #[command(flatten)]
    pub lifecycle: LifecycleOverlay,

    /// Per-actor ring-capacity knobs (issue 1990): `--actor-log-ring-capacity` /
    /// `--actor-trace-ring-capacity` / `--actor-trace-ring-max-size`, shadowing the
    /// `AETHER_ACTOR_*` env. Both full-stack chassis resolve `ActorRingConfig` off
    /// the shared stack (issue 3882 flattened the derive-emitted overlay here).
    #[command(flatten)]
    pub actor_ring: ActorRingOverlay,
    /// Scheduler hot-path tuning knobs (issue 2485): `--scheduler-*`, shadowing the
    /// `AETHER_SPIN_WINDOW_USEC` / `AETHER_LOCAL_*` / `AETHER_*_RECRUIT_*` env. Both
    /// full-stack chassis resolve `SchedulerTuningConfig` off the shared stack
    /// (issue 3882).
    #[command(flatten)]
    pub scheduler: SchedulerTuningOverlay,
    /// Settlement-patience backstop (issue 2062): `--settlement-cap-secs`, shadowing
    /// `AETHER_SETTLEMENT_CAP_SECS`. Resolved for both the settlement gates and the
    /// teardown budget (issue 3882 flattened its overlay here).
    #[command(flatten)]
    pub settlement: SettlementOverlay,

    /// `--rpc-port` shadows `AETHER_RPC_PORT` — the `aether.rpc.server` bind
    /// port. Absent → the member's `None` default, so desktop / headless skip
    /// the RPC server entirely; the hub applies its own `DEFAULT_RPC_PORT`
    /// fallback at its compose site.
    #[command(flatten)]
    pub rpc: RpcServerOverlay,
}

/// Desktop chassis CLI root.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-substrate",
    about = "Desktop chassis — winit window + wgpu render + cpal audio. ADR-0035 / ADR-0090.",
    long_about = "Desktop chassis — winit window + wgpu render + cpal audio. ADR-0035 / ADR-0090.\n\n\
        Each flag below carries its resolved env key and default in brackets; unset flags fall \
        through to env then the default. For the full source-resolved value of every knob use \
        --print-config, and for this binary's linked caps and build provenance use --describe.",
    after_help = env_only_after_help()
)]
pub struct DesktopCli {
    #[command(flatten)]
    pub common: CommonOverlay,
    #[command(flatten)]
    pub audio: AudioOverlay,
    /// Render cap tuning (desktop composes the wgpu render cap):
    /// `--render-vertex-buffer-bytes`, shadowing `AETHER_RENDER_VERTEX_BUFFER_BYTES`
    /// (issue 3882 flattened its overlay here; headless composes the nop render cap,
    /// which resolves no `RenderTuningConfig`, so it carries no render flag).
    #[command(flatten)]
    pub render: RenderTuningOverlay,
    /// Desktop window knobs: `--window-mode`, `--window-title`.
    #[command(flatten)]
    pub window: WindowOverlay,

    /// Sectioned TOML chassis config file. Values from this file sit
    /// below env and argv in the source stack.
    #[arg(long = "config", value_name = "PATH")]
    #[stage(skip)]
    pub config: Option<String>,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "print-config")]
    #[stage(skip)]
    pub print_config: bool,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps,
    /// build provenance) as JSON and exit before boot (ADR-0115, issue
    /// 1953). The hub's binary store forks `<binary> --describe` once at
    /// upload time to capture what a stored binary is.
    #[arg(long = "describe")]
    #[stage(skip)]
    pub describe: bool,
}

/// Headless chassis CLI root.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-substrate-headless",
    about = "Headless chassis — std-timer tick driver, nop render. ADR-0035 / ADR-0090.",
    long_about = "Headless chassis — std-timer tick driver, nop render. ADR-0035 / ADR-0090.\n\n\
        Each flag below carries its resolved env key and default in brackets; unset flags fall \
        through to env then the default. For the full source-resolved value of every knob use \
        --print-config, and for this binary's linked caps and build provenance use --describe.",
    after_help = env_only_after_help()
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
    #[stage(skip)]
    pub config: Option<String>,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "print-config")]
    #[stage(skip)]
    pub print_config: bool,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps,
    /// build provenance) as JSON and exit before boot (ADR-0115, issue
    /// 1953). The hub's binary store forks `<binary> --describe` once at
    /// upload time to capture what a stored binary is.
    #[arg(long = "describe")]
    #[stage(skip)]
    pub describe: bool,
}

/// Hub chassis CLI root — coordinator-only, no full-stack caps.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-substrate-hub",
    about = "Hub chassis — coordinator between aether-mcp + substrate fleet. ADR-0073.",
    long_about = "Hub chassis — coordinator between aether-mcp + substrate fleet. ADR-0073.\n\n\
        Each flag below carries its resolved env key and default in brackets; unset flags fall \
        through to env then the default. For the full source-resolved value of every knob use \
        --print-config, and for this binary's linked caps and build provenance use --describe.",
    after_help = hub_env_only_after_help()
)]
pub struct HubCli {
    /// `--rpc-port` shadows `AETHER_RPC_PORT` — the `aether.rpc.server` bind
    /// port (the hub applies its `DEFAULT_RPC_PORT`, 8901, when unset).
    #[command(flatten)]
    pub rpc: RpcServerOverlay,

    /// Engines-cap knobs — the liveness-heartbeat tuning
    /// (`--hub-heartbeat-interval-secs` / `--hub-heartbeat-miss-limit`,
    /// issue 1339). Flattened from the derive-emitted overlay.
    #[command(flatten)]
    pub engine: EngineOverlay,

    /// Per-actor ring-capacity knobs (issue 1990): `--actor-*`. The hub resolves
    /// `ActorRingConfig` off its own source stack for the actors its registry hosts
    /// (issue 3882 flattened the overlay here).
    #[command(flatten)]
    pub actor_ring: ActorRingOverlay,
    /// Scheduler hot-path tuning knobs (issue 2485): `--scheduler-*`. The hub
    /// resolves `SchedulerTuningConfig` off its own source stack (issue 3882).
    #[command(flatten)]
    pub scheduler: SchedulerTuningOverlay,
    /// Settlement-patience backstop (issue 2062): `--settlement-cap-secs`. The hub
    /// resolves `SettlementConfig` for its own teardown budget (issue 3882).
    #[command(flatten)]
    pub settlement: SettlementOverlay,

    /// Sectioned TOML chassis config file. Values from this file sit
    /// below env and argv in the source stack.
    #[arg(long = "config", value_name = "PATH")]
    #[stage(skip)]
    pub config: Option<String>,

    /// Print every config knob (source-resolved value, default, doc)
    /// and exit before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "print-config")]
    #[stage(skip)]
    pub print_config: bool,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps,
    /// build provenance) as JSON and exit before boot (ADR-0115, issue
    /// 1953). The hub's binary store forks `<binary> --describe` once at
    /// upload time to capture what a stored binary is.
    #[arg(long = "describe")]
    #[stage(skip)]
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

    use super::{
        ActorRingOverlay, CommonOverlay, DesktopCli, EngineOverlay, HeadlessCli, HttpOverlay, HubCli,
        RenderTuningOverlay, RpcServerOverlay, SchedulerTuningOverlay, SettlementOverlay, TickOverlay,
    };
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
        // Desktop composes the wgpu render cap, so its `RenderTuningConfig` overlay
        // is flattened only here, not into the shared `CommonOverlay` (issue 3882).
        expected.extend(overlay_flags::<RenderTuningOverlay>());
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
    fn derived_flag_help_carries_doc_env_and_default() {
        // Tripwire: the `#[derive(Config)]` overlay must forward the domain
        // field's first rustdoc sentence (joined across the source hard-wrap,
        // cut at the first sentence boundary) and append the confique-resolved
        // env key plus the declared default onto each flag's clap help, and
        // stamp the ms_duration hint's typed value name (issue 3862). clap
        // sees none of these on its own — env resolution is confique-side,
        // the default lives on the Layer, and the value name would default
        // to the id — so a regression that dropped the forwarding leaves a
        // bare `--http-timeout-ms <http_timeout_ms>`. Walked through the
        // same clap introspection a `--help` render uses.
        let command = HttpOverlay::augment_args(clap::Command::new("probe"));
        let arg = command
            .get_arguments()
            .find(|arg| arg.get_long() == Some("http-timeout-ms"))
            .expect("--http-timeout-ms is present");
        let help = arg.get_help().map(ToString::to_string).expect("flag carries help");
        // The consumer-grade first sentence is forwarded verbatim as the flag's
        // help lead, ahead of the appended env key and default.
        assert!(
            help.contains("Default per-request timeout in milliseconds."),
            "first rustdoc sentence forwarded as help lead: {help}"
        );
        assert!(help.contains("[env: AETHER_HTTP_TIMEOUT_MS]"), "resolved env key annotated: {help}");
        assert!(help.contains("[default: 30000]"), "declared default annotated (separators stripped): {help}");
        let value_names: Vec<String> =
            arg.get_value_names().unwrap_or_default().iter().map(ToString::to_string).collect();
        assert_eq!(value_names, ["MILLIS"], "ms_duration hint stamps the typed value name");
    }

    #[test]
    fn hub_root_flags_equal_composed_overlay_set() {
        // The hub composes the engines cap plus the RPC server; `--rpc-port`
        // now rides the derive-emitted `RpcServerOverlay` (#3849) like every
        // other flag, alongside the meta flags. Issue 3882 flattened the three
        // tuning overlays the hub resolves off its own source stack (actor ring /
        // scheduler / settlement).
        let mut expected = overlay_flags::<EngineOverlay>();
        expected.extend(overlay_flags::<RpcServerOverlay>());
        expected.extend(overlay_flags::<ActorRingOverlay>());
        expected.extend(overlay_flags::<SchedulerTuningOverlay>());
        expected.extend(overlay_flags::<SettlementOverlay>());
        expected.extend(meta_flags());
        assert_eq!(long_flags(&HubCli::command()), expected);
    }
}
