//! Shared chassis-CLI machinery (ADR-0090 unit d, issue 1258): the pieces every
//! chassis root composes from — the [`CommonOverlay`] full-stack cap bundle, the
//! [`ChassisMeta`] source-selecting flags, and the [`ChassisCli`] trait that
//! assembles the file+argv source stack. Each chassis crate owns its own root
//! (`DesktopCli` / `HeadlessCli` / `HubCli` live in their chassis crates); this
//! crate carries what those roots share.
//!
//! A chassis bin calls `<Cli>::parse()` and threads the resolved overlays through
//! `*Env::resolve(cli)`; each overlay's `into_layer()` writes argv-set fields into
//! a partial `<*ConfigLayer as confique::Config>::Layer`, which the cap's
//! `from_argv_then_env(...)` then preloads ahead of `.env()` so argv beats env
//! beats literal defaults. Absent flags resolve `None` and fall through to
//! env-only resolution — boot is byte-identical when argv is empty.
//!
//! ADR-0156 §5 (issue 3872): staging each overlay onto the source stack is
//! **derived**, not hand-maintained. `#[derive(aether_substrate::Config)]` emits
//! a leaf `StageArgv` on every `*Overlay`, and each chassis root carries
//! `#[derive(aether_substrate::StageArgv)]` — the container half that delegates
//! to every field's `stage_argv`. A chassis then stages its whole CLI in one
//! `cli.stage_argv(&mut sources)` call, so adding an overlay field to a root IS
//! staging it. Non-overlay meta fields (`config` / `print_config` / `describe`)
//! carry `#[stage(skip)]`; an unannotated non-overlay field fails to compile,
//! and a staged-but-never-composed overlay fails boot loudly
//! (`ConfigSources::validate_no_orphan_argv`).
//!
//! Flag naming is mechanical: strip an `AETHER_` (or top-level)
//! prefix, lowercase, hyphenate. `AETHER_HTTP_TIMEOUT_MS` →
//! `--http-timeout-ms`, `AETHER_PROCESS_TIMEOUT_MS` → `--process-timeout-ms`.
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
//! each root's `after_help` (`crate::boot::env_only_after_help`) by harvesting the
//! derive-emitted overlays' own clap help, so the section carries each knob's doc
//! + env + default and cannot drift from the registry.

use std::collections::BTreeSet;

use aether_fs::NamespaceRootsOverlay;
use aether_harness_substrate::SettlementOverlay;
use aether_http::{HttpOverlay, HttpServerOverlay};
use aether_lifecycle::LifecycleOverlay;
use aether_process::ProcessOverlay;
use aether_rpc::RpcServerOverlay;
use aether_substrate::config::{ConfigError, ConfigSources, StageArgv};
use clap::Args;

use crate::boot::{ActorRingOverlay, ChassisBootOverlay, SchedulerTuningOverlay, load_chassis_config};

/// The three source-selecting meta flags every chassis root carries. They name
/// the file source and the print/describe exits, so they belong to no cap
/// member and take no part in argv staging (`#[stage(skip)]` where flattened).
#[derive(Args, Debug, Default, Clone)]
pub struct ChassisMeta {
    /// Sectioned TOML chassis config file. Values from this file sit below env
    /// and argv in the source stack.
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<String>,

    /// Print every config knob (source-resolved value, default, doc) and exit
    /// before boot (ADR-0090 §4 discovery dump).
    #[arg(long = "print-config")]
    pub print_config: bool,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps, build
    /// provenance) as JSON and exit before boot (ADR-0115, issue 1953). The
    /// hub's binary store forks `<binary> --describe` once at upload time to
    /// capture what a stored binary is.
    #[arg(long = "describe")]
    pub describe: bool,
}

/// A chassis CLI root: a derived [`StageArgv`] over its per-cap overlay fields
/// plus the shared [`ChassisMeta`] flags. [`into_sources`](Self::into_sources)
/// is the whole file+argv source-stack assembly every resolver opens with —
/// load the `--config` file into a [`ConfigSources`], then stage the root's
/// derived argv layers onto it (argv > env > file > default).
pub trait ChassisCli: StageArgv + Sized {
    /// The root's flattened [`ChassisMeta`] — the file source and print/describe
    /// exits.
    fn meta(&self) -> &ChassisMeta;

    /// Assemble the source stack: the loaded `--config` file plus every cap
    /// member's typed argv overlay, staged in one derived [`StageArgv`] call off
    /// the CLI declaration itself.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the `--config` file fails to load or parse.
    fn into_sources(self) -> Result<ConfigSources, ConfigError> {
        let mut sources = ConfigSources::new(load_chassis_config(self.meta().config.clone())?);
        self.stage_argv(&mut sources);
        Ok(sources)
    }
}

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
    pub fs: NamespaceRootsOverlay,
    /// One-shot exec cap knobs (ADR-0157): `--process-allowlist` /
    /// `--process-max-in-flight` / `--process-timeout-ms`, shadowing the
    /// `AETHER_PROCESS_*` env.
    #[command(flatten)]
    pub process: ProcessOverlay,
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

/// Every `--long` flag a clap command declares. The building block chassis crates
/// assemble their root-flag checkability tests from (see [`overlay_flags`]).
#[must_use]
pub fn long_flags(command: &clap::Command) -> BTreeSet<String> {
    command.get_arguments().filter_map(|arg| arg.get_long().map(str::to_owned)).collect()
}

/// The long flags an [`Args`] overlay contributes, gathered by augmenting a
/// throwaway command with it — the same flags a chassis root gets by
/// `#[command(flatten)]`-ing the overlay. A chassis crate builds its root's
/// expected flag set by unioning `overlay_flags::<T>()` over the overlays it
/// composes, plus [`meta_flags`].
#[must_use]
pub fn overlay_flags<T: Args>() -> BTreeSet<String> {
    long_flags(&T::augment_args(clap::Command::new("probe")))
}

/// The source-selecting meta flags every chassis root carries directly (they
/// name the file source and the print/describe exits, so they belong to no cap
/// member). Derived from the [`ChassisMeta`] `Args` group itself, the same
/// flatten a root gets, so the expected set cannot drift from the declaration.
#[must_use]
pub fn meta_flags() -> BTreeSet<String> {
    overlay_flags::<ChassisMeta>()
}

#[cfg(test)]
mod tests {
    use super::HttpOverlay;
    use clap::Args as _;

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
}
