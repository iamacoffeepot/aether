//! Shared `Builder` boot fragments for the desktop and headless
//! chassis. Both `Chassis::build` impls pre-extraction wired the same
//! 10-cap base (handle, log, trace, input, component-host, fs, http,
//! tcp + the aborter + worker count) and the same optional RPC
//! server tail, with only their renderer + window stack differing.
//! The duplicate-code check flagged the parallel chains as duplicated code; this module
//! pulls the shared scaffolding out so each chassis declares only
//! the parts that genuinely differ.
//!
//! The hub and substrate-harness chassis don't share this base (hub is a
//! minimal RPC-only chassis, substrate-harness drives a loopback), so the
//! helper module stays scoped to the two full-stack chassis.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aether_actor::log::DEFAULT_RING_CAP;
use aether_actor::trace::{DEFAULT_TRACE_RING_CAP, DEFAULT_TRACE_RING_MAX_CAP};
use aether_codec::frame::install_max_frame_size;
use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_fs::{FsCapability, NamespaceRoots};
use aether_game::{GameGatewayCapability, GameGatewayConfig, GameGatewayParams};
use aether_http::HttpCapability;
use aether_input::InputCapability;
use aether_inventory::InventoryCapability;
use aether_kinds::{Shutdown, Tick};
use aether_lifecycle::{LifecycleGraphData, LifecycleParams};
use aether_process::{ProcessCapability, ProcessParams};
use aether_rpc::{FrameSizeConfig, FrameSizeOverlay, PeerKind, RpcServerCapability, RpcServerParams};
use aether_substrate::chassis::builder::Builder;
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::{
    BootableChassis, BuildProvenance, Chassis, PreludeAction, PreludeFlags, run_chassis_prelude,
};
use aether_substrate::config::{
    ConfigError, ConfigMember, ConfigSources, KnobKind, KnobRecord, RingCapacities, SchedulerTuning,
};
use aether_substrate::runtime::lifecycle::FatalAborter;

use aether_tcp::TcpCapability;
use aether_text::TextCapability;
use aether_trace::TraceDispatchCapability;

use crate::autoload::{AutoloadComponent, boot_manifest_autoload};

/// Env fallback for the chassis config-file path. The path is
/// meta-config: it selects the file source and does not change the file
/// layer's lower precedence relative to ordinary `AETHER_*` env knobs.
pub const CONFIG_FILE_ENV: &str = "AETHER_CONFIG_FILE";

/// Chassis-direct env knobs that aren't `#[derive(Config)]` members — the
/// residual hand records the composition-derived
/// [`ConfigManifest`](aether_substrate::config::ConfigManifest) can't own.
///
/// After #3849 (`AETHER_RPC_PORT` → the `RpcServerConfig` member; the runtime
/// log / panic-hook knobs → the chassis-declared [`RuntimeConfig`] member) and
/// #3850 (`AETHER_MAX_FRAME_SIZE` → the [`FrameSizeConfig`] member, pushed into
/// the codec by [`install_frame_size`]), every former residual moved onto a
/// derive-`Config` member resolved through the source stack. The meta-knob that
/// selects the file source is the sole survivor — it *cannot* live in the file
/// it selects.
///
/// Registered as a [`KnobRecord`] so the unknown-`AETHER_*` sweep doesn't flag
/// it and the `--print-config` dump lists it (ADR-0090 §1/§4). The scheduler
/// hot-path, chassis boot, window, tick, RPC-port, frame-size, and runtime knobs
/// are covered by their derive-emitted `*Layer::META`s the manifest walks.
pub const CHASSIS_KNOBS: &[KnobRecord] = &[KnobRecord {
    env_key: CONFIG_FILE_ENV,
    doc: "Path to a sectioned TOML chassis config file; overridden by --config <PATH>.",
    default: None,
    kind: KnobKind::HandRegistered,
}];

/// Resolve the optional chassis config-file path from argv first, then
/// `AETHER_CONFIG_FILE`. Empty values are treated as absent.
#[must_use]
pub fn chassis_config_path(argv: Option<String>) -> Option<PathBuf> {
    argv.filter(|path| !path.is_empty()).map(PathBuf::from).or_else(|| {
        // This is the central meta-config read for selecting the config
        // file source, not a subsystem reading its own knob.
        #[allow(clippy::disallowed_methods)]
        env::var_os(CONFIG_FILE_ENV).filter(|path| !path.as_os_str().is_empty()).map(PathBuf::from)
    })
}

/// Read and parse an explicitly supplied sectioned TOML chassis config
/// file. Missing or malformed files are hard boot errors because the
/// operator asked for this source.
///
/// # Errors
///
/// Returns [`ConfigError`] when the file cannot be read or parsed as TOML.
pub fn load_config_file(path: &Path) -> Result<toml::Table, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| ConfigError::config_file(path, source))?;
    text.parse::<toml::Table>().map_err(|source| ConfigError::config_file(path, source))
}

/// Load the chassis config file selected by argv or
/// `AETHER_CONFIG_FILE`, returning `None` when neither is set.
///
/// # Errors
///
/// Returns [`ConfigError`] when an explicitly supplied file cannot be
/// read or parsed.
pub fn load_chassis_config(argv: Option<String>) -> Result<Option<toml::Table>, ConfigError> {
    chassis_config_path(argv).map(|path| load_config_file(&path)).transpose()
}

/// Per-actor ring-capacity knob (issue 1990, ADR-0081 / ADR-0086). The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `ActorRingConfigLayer`, the clap-shaped `ActorRingOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims (ADR-0090 unit g). Resolved once at chassis
/// boot and lowered via [`Self::to_ring_capacities`] to the `Copy`
/// [`RingCapacities`] the chassis builder threads down the spawn path.
///
/// `env_prefix = "AETHER_ACTOR"` joins the field env keys; the explicit
/// `env =` overrides pin the historical names — the log key
/// (`AETHER_ACTOR_LOG_RING_SIZE`) is the one ADR-0081 already documented
/// (previously documented-but-dead; this is what wires it), the trace
/// floor key (`AETHER_ACTOR_TRACE_RING_SIZE`) its sibling, and the trace
/// ceiling key (`AETHER_ACTOR_TRACE_RING_MAX_SIZE`) the size a saturating
/// trace ring grows to before it resumes drop-oldest.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_ACTOR", cli_prefix = "actor")]
pub struct ActorRingConfig {
    /// Per-actor log-ring capacity in entries.
    ///
    /// Default [`DEFAULT_RING_CAP`]. Zero clamps to 1 inside
    /// `ActorLogRing::with_capacity`.
    #[config(env = "AETHER_ACTOR_LOG_RING_SIZE", default = 1024)]
    pub log_ring_capacity: usize,
    /// Starting per-actor trace-ring capacity in entries.
    ///
    /// The per-actor (and chassis-host) trace-ring *floor* — the size each
    /// ring starts at (default [`DEFAULT_TRACE_RING_CAP`]). Zero clamps to
    /// 1 inside `ActorTraceRing::with_growth`.
    #[config(env = "AETHER_ACTOR_TRACE_RING_SIZE", default = 4096)]
    pub trace_ring_capacity: usize,
    /// Maximum trace-ring capacity in entries before it resumes dropping oldest.
    ///
    /// The ceiling a saturating trace ring grows to (default
    /// [`DEFAULT_TRACE_RING_MAX_CAP`]). A value below the floor clamps up
    /// to the floor inside `ActorTraceRing::with_growth`.
    #[config(env = "AETHER_ACTOR_TRACE_RING_MAX_SIZE", default = 65536)]
    pub trace_ring_max_size: usize,
}

impl Default for ActorRingConfig {
    fn default() -> Self {
        Self {
            log_ring_capacity: DEFAULT_RING_CAP,
            trace_ring_capacity: DEFAULT_TRACE_RING_CAP,
            trace_ring_max_size: DEFAULT_TRACE_RING_MAX_CAP,
        }
    }
}

impl ActorRingConfig {
    /// Lower the resolved knob to the `Copy` [`RingCapacities`] the
    /// chassis builder threads down the spawn path.
    #[must_use]
    pub fn to_ring_capacities(&self) -> RingCapacities {
        RingCapacities {
            log: self.log_ring_capacity,
            trace: self.trace_ring_capacity,
            trace_max: self.trace_ring_max_size,
        }
    }
}

/// The nine scheduler hot-path tuning knobs (issue 2485), resolved once
/// at chassis boot and lowered via [`Self::to_scheduler_tuning`] to the
/// `Copy` [`SchedulerTuning`] the chassis builder installs into the
/// scheduler's process-global before the pool starts. The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `SchedulerTuningConfigLayer`, the clap-shaped `SchedulerTuningOverlay`,
/// the `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` / `try_*` shims (ADR-0090 unit d/g) — replacing
/// the nine hand-registered `KnobRecord`s the scheduler read directly from
/// env. A garbage value for a concrete knob hard-errors at boot (ADR-0090
/// §4); the `nonzero` knobs coerce a resolved `0` to their default; the
/// three adaptive knobs are `Option` (unset / `< 1` → the measured /
/// derived behaviour).
///
/// Each field pins its historical `AETHER_*` env key explicitly (the keys
/// don't follow the `env_prefix` shape); the Rust identifiers spell the
/// unit out (`micros` / `nanos`) while the env keys stay byte-for-byte.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER", cli_prefix = "scheduler")]
pub struct SchedulerTuningConfig {
    /// Microseconds a worker spins for more work before parking.
    ///
    /// The route-to-spinner spin-window before a worker parks (default
    /// `50`). A `0` is valid (no spin).
    #[config(env = "AETHER_SPIN_WINDOW_USEC", default = 50)]
    pub spin_window_micros: u64,
    /// Maximum tasks a worker keeps on its local queue before spilling.
    ///
    /// The deque-length backstop (default `256`; a resolved `0` coerces to
    /// the default).
    #[config(env = "AETHER_LOCAL_STICKY_MAX", default = 256, nonzero)]
    pub local_sticky_max: usize,
    /// Microseconds a worker keeps chaining work locally before spilling; unset auto-tunes.
    ///
    /// The keep-local time valve. Unset → adaptive (derived from the
    /// measured handoff cost); `0` disables the valve (pure
    /// inline-cascade).
    #[config(env = "AETHER_LOCAL_TIME_BUDGET_US")]
    pub time_budget_micros: Option<u64>,
    /// Allow idle workers to steal tasks from other workers' queues.
    ///
    /// Default `false` — owner-only. Accepts `1`/`true`/`yes`.
    #[config(env = "AETHER_PEER_STEAL", default = false)]
    pub peer_steal: bool,
    /// How often a keep-local chain is forced back to the shared queue.
    ///
    /// The every-K injector backstop for keep-local chains (default `64`;
    /// a resolved `0` coerces to it).
    #[config(env = "AETHER_LOCAL_CHAIN_BACKSTOP", default = 64, nonzero)]
    pub local_chain_backstop: u32,
    /// Pin the cross-worker handoff-cost estimate in nanoseconds; unset auto-tunes.
    ///
    /// Pins the estimate and freezes live refinement. Unset / `< 1` →
    /// boot-probed and live-refined ([`Self::to_scheduler_tuning`] filters
    /// `< 1` to `None`).
    #[config(env = "AETHER_HANDOFF_COST_NS")]
    pub handoff_cost_nanos: Option<u64>,
    /// Minimum number of fresh groups before a flush recruits sibling workers.
    ///
    /// The minimum fresh-group count for a flush to broadcast-recruit
    /// siblings (default `9`; `0` coerces to it).
    #[config(env = "AETHER_BLOB_RECRUIT_MIN", default = 9, nonzero)]
    pub blob_recruit_min: usize,
    /// Maximum number of sibling workers a single flush recruits.
    ///
    /// The cap on sibling copies a single flush injects when recruiting
    /// (default `32`; `0` coerces to it).
    #[config(env = "AETHER_BLOB_RECRUIT_MAX", default = 32, nonzero)]
    pub blob_recruit_max: usize,
    /// Pin the recruit wake break-even cost in nanoseconds; unset auto-tunes.
    ///
    /// Pins the break-even and freezes live refinement. Unset / `< 1` →
    /// the box-measured handoff cost ([`Self::to_scheduler_tuning`]
    /// filters `< 1` to `None`).
    #[config(env = "AETHER_WAKE_COST_NANOS")]
    pub wake_cost_nanos: Option<u64>,
}

impl Default for SchedulerTuningConfig {
    fn default() -> Self {
        // These literals must equal `SchedulerTuning::default()`;
        // `scheduler_tuning_defaults_match` guards the pair.
        Self {
            spin_window_micros: 50,
            local_sticky_max: 256,
            time_budget_micros: None,
            peer_steal: false,
            local_chain_backstop: 64,
            handoff_cost_nanos: None,
            blob_recruit_min: 9,
            blob_recruit_max: 32,
            wake_cost_nanos: None,
        }
    }
}

impl SchedulerTuningConfig {
    /// Lower the resolved knob to the `Copy` [`SchedulerTuning`] the
    /// chassis builder installs before `Pool::start`. The only logic this
    /// crate owns is the `< 1 → None` filter on the two pin knobs
    /// (`handoff_cost_nanos` / `wake_cost_nanos`): a `0` pin would disable
    /// the gate it guards, so it falls through to the measured cost (the
    /// concrete knobs' `< 1 → default` is handled by the `nonzero` hint;
    /// `time_budget_micros`'s `0` is a meaningful "disable the valve" and
    /// passes through).
    #[must_use]
    pub fn to_scheduler_tuning(&self) -> SchedulerTuning {
        SchedulerTuning {
            spin_window_micros: self.spin_window_micros,
            local_sticky_max: self.local_sticky_max,
            time_budget_micros: self.time_budget_micros,
            peer_steal: self.peer_steal,
            local_chain_backstop: self.local_chain_backstop,
            handoff_cost_nanos: self.handoff_cost_nanos.filter(|&n| n >= 1),
            blob_recruit_min: self.blob_recruit_min,
            blob_recruit_max: self.blob_recruit_max,
            wake_cost_nanos: self.wake_cost_nanos.filter(|&n| n >= 1),
        }
    }
}

// Issue #3765: `SettlementConfig` (the `AETHER_SETTLEMENT_CAP_SECS`
// knob) rehomed to `aether-harness-substrate`, its primary consumer; the
// chassis teardown resolution below reads the same knob through the
// re-import.
use aether_harness_substrate::{DEFAULT_HEIGHT, DEFAULT_WIDTH};
pub use aether_harness_substrate::{SettlementConfig, SettlementConfigLayer};

/// Render-size knob for the standalone substrate-harness binary
/// (`AETHER_SUBSTRATE_HARNESS_SIZE=WxH`). Mirrors the single-field
/// `SettlementConfig` shape: a `#[derive(aether_substrate::Config)]`
/// struct resolved `from_env()` and lowered to `(u32, u32)` by
/// [`Self::to_size`]. Lives binary-side (issue #3765) — the in-process
/// harness sizes through its builder, not process env.
///
/// The explicit `env =` pin is belt-and-suspenders against a future field
/// rename, matching how `ActorRingConfig` pins its historical keys.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_SUBSTRATE_HARNESS", cli_prefix = "substrate-harness")]
pub struct RenderSizeConfig {
    /// Offscreen render width and height in pixels; unset falls back to 800x600.
    ///
    /// Render dimensions for the offscreen wgpu surface, given as
    /// `width x height`. Falls back to `800x600` on missing/unparseable
    /// input with a warn log.
    #[config(env = "AETHER_SUBSTRATE_HARNESS_SIZE")]
    pub size: Option<String>,
}

impl RenderSizeConfig {
    /// Lower the resolved knob to `(width, height)` pixels. Preserves the
    /// `parse_size_env` semantics verbatim: missing env var, missing `x`
    /// separator, non-numeric parts, or a zero dimension all fall back to
    /// [`DEFAULT_WIDTH`] × [`DEFAULT_HEIGHT`] with a `warn` log.
    #[must_use]
    pub fn to_size(&self) -> (u32, u32) {
        let Some(raw) = self.size.as_deref() else {
            return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
        };
        if let Some((w, h)) = raw.split_once('x') {
            match (w.parse::<u32>(), h.parse::<u32>()) {
                (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
                _ => {
                    tracing::warn!(
                        target: "aether_substrate::boot",
                        value = %raw,
                        "AETHER_SUBSTRATE_HARNESS_SIZE unparseable — falling back to default",
                    );
                    (DEFAULT_WIDTH, DEFAULT_HEIGHT)
                }
            }
        } else {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                "AETHER_SUBSTRATE_HARNESS_SIZE missing 'x' separator — falling back to default",
            );
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        }
    }
}

/// Issue #2509: resolve the instanced-actor teardown close-done gate's
/// cumulative-patience budget from the shared `AETHER_SETTLEMENT_CAP_SECS`
/// knob (`SettlementConfig::to_cap`, including its `0 → Duration::MAX`
/// "wait forever" sentinel). Each chassis threads the result into the
/// substrate `Builder` via `with_teardown_budget`, so one knob covers both
/// the settlement gates and the teardown gate. A crate-root re-export
/// keeps it reachable from the chassis bins (which cannot see the private
/// `SettlementConfig`).
#[must_use]
pub fn resolve_teardown_budget() -> Duration {
    SettlementConfig::from_env().to_cap()
}

/// Shared boot knobs for the desktop and headless chassis
/// (ADR-0090 §1/§2 applied to the chassis's own knobs). The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `ChassisBootConfigLayer`, the clap-shaped `ChassisBootOverlay`,
/// the `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` / `try_*` shims — mirrors [`ActorRingConfig`].
///
/// `env_prefix = "AETHER"` joins the field env keys; explicit
/// `cli_long` overrides pin the historical flag names so existing
/// scripts and operators are unaffected.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER", cli_prefix = "chassis")]
pub struct ChassisBootConfig {
    /// Worker-pool thread count; unset uses one fewer than the available cores.
    ///
    /// Unset → `available_parallelism()-1`, min 1. `Option<usize>`
    /// soft-parses (unparseable → `None`, matching the old
    /// `parse_workers_env` fallback). The 0→1 clamp logic lives in
    /// [`Self::to_workers`].
    #[config(cli_long = "workers")]
    pub workers: Option<usize>,
    /// Path to a JSON manifest of components to auto-load at boot.
    ///
    /// The runtime twin of the standalone-bundle compile-time pack;
    /// injected by the engines cap on a `spawn_substrate` carrying
    /// components. `Option<String>` filters empty → `None`, exactly
    /// matching `boot_manifest_from_env`.
    #[config(cli_long = "boot-manifest")]
    pub boot_manifest: Option<String>,
}

impl ChassisBootConfig {
    /// Lower the resolved `workers` knob to the pool-size `Option<usize>`
    /// the chassis builder's `with_workers` takes. The 0→1 clamp is the
    /// only piece of logic this crate owns (the rest is pure field reads):
    /// `0` is invalid for the pool (it requires at least one worker) and
    /// users who set it almost certainly meant "any" (i.e. the system
    /// default), so we clamp + warn rather than hard-error.
    pub fn to_workers(&self) -> Option<usize> {
        match self.workers {
            None => None,
            Some(0) => {
                tracing::warn!(
                    target: "aether_substrate::boot",
                    "AETHER_WORKERS=0 — clamping to 1",
                );
                Some(1)
            }
            Some(n) => Some(n),
        }
    }
}

/// Resolve the [`FrameSizeConfig`] member off the assembled source stack and
/// push it set-once into `aether-codec` (ADR-0156 §6, #3850). Each chassis calls
/// this once during config resolution — before the RPC server binds or dials, so
/// it runs before any framing. `aether-codec` sits below the config system and
/// cannot pull the knob itself; this is the push half of the inversion, the
/// codec's [`install_max_frame_size`] the receiving half.
///
/// # Errors
///
/// Returns [`ConfigError`] when the `AETHER_MAX_FRAME_SIZE` member fails to
/// resolve (a garbage value hard-errors at boot, ADR-0090 §4).
pub fn install_frame_size(sources: &mut ConfigSources) -> Result<(), ConfigError> {
    install_max_frame_size(sources.resolve::<FrameSizeConfig>()?.to_max_frame_size());
    Ok(())
}

/// The substrate runtime knobs (ADR-0156 §6, #3849): the tracing log-filter
/// directive plus the three panic-hook knobs, migrated off the hand-registered
/// `RUNTIME_KNOBS` slice onto this chassis-declared `#[derive(aether_substrate::Config)]`
/// member so they join the composition-derived aggregate — the known-keys sweep
/// and `--print-config` dump — like any other member instead of riding
/// [`chassis_residual_knobs`]. Each chassis resolves it before Compose and
/// declares its membership via `declare_config_member` (its value applies off
/// the builder, so it is declaration-only; folded into [`with_common_caps`] and
/// the hub compose).
///
/// `env_prefix = "AETHER"` with an explicit `env =` per field pins each
/// historical key byte-for-byte. Only [`log_filter`](Self::log_filter) is
/// re-applied after resolution (via
/// [`apply_filter`](aether_substrate::runtime::log_install::apply_filter)): the
/// subscriber installs an env-or-`info` filter at boot, before the config file
/// loads, so a `[runtime]` file directive needs a re-apply. The three
/// panic-hook fields declare their keys for the aggregate; the process-level
/// panic hook keeps reading them from env directly (installed at boot, fired at
/// panic time — below the actor/config layer, above no cap).
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER", cli_prefix = "runtime")]
pub struct RuntimeConfig {
    /// Log-level filter directive for the substrate; defaults to info.
    ///
    /// A tracing `EnvFilter` directive for the substrate subscriber stack.
    /// Re-applied after full resolution so a `[runtime]` config-file value
    /// below env takes effect.
    #[config(env = "AETHER_LOG_FILTER", default = "info")]
    pub log_filter: String,
    /// Capture a backtrace on a substrate panic.
    ///
    /// Forces backtrace capture without flipping the process-wide
    /// `RUST_BACKTRACE`. Presence-tested by the panic hook; declared here
    /// for the aggregate.
    #[config(env = "AETHER_BACKTRACE")]
    pub backtrace: Option<String>,
    /// Disable writing a crash-dump file on a substrate panic.
    ///
    /// Disables the JSONL crash-dump path (the tracing event still fires).
    /// Consumed by the panic hook's own lenient env read; declared here
    /// for the aggregate.
    #[config(env = "AETHER_CRASH_LOG_DISABLE")]
    pub crash_log_disable: Option<String>,
    /// Directory for crash-dump files; unset uses the platform data dir.
    ///
    /// Overrides the crash-dump base directory (unset →
    /// `$XDG_DATA_HOME/aether/crash/`, then
    /// `$HOME/.local/share/aether/crash/`). Consumed by the panic hook;
    /// declared here for the aggregate.
    #[config(env = "AETHER_CRASH_LOG_DIR")]
    pub crash_log_dir: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        // Matches the unset resolution: the subscriber's `info` fallback and no
        // panic-hook overrides. Tests that construct an `Env` directly use this.
        Self { log_filter: "info".to_owned(), backtrace: None, crash_log_disable: None, crash_log_dir: None }
    }
}

/// The residual hand-registered knobs the composition-derived
/// [`ConfigManifest`](aether_substrate::config::ConfigManifest) can't own — the
/// sole survivor is the `AETHER_CONFIG_FILE` meta-knob that selects the file
/// source (so it cannot live in the file), alongside the orphaned codec
/// frame-size knob (retired by #3850). Every other former residual — the RPC
/// port and the runtime log / panic-hook knobs — moved onto derive-`Config`
/// members (#3849), so they now ride the manifest walk. Folded in beside the
/// manifest metas by every chassis's known-keys sweep and `--print-config`
/// dump ([`ConfigManifest::known_keys`](aether_substrate::config::ConfigManifest::known_keys)
/// / [`dump`](aether_substrate::config::ConfigManifest::dump)).
#[must_use]
pub fn chassis_residual_knobs() -> Vec<KnobRecord> {
    CHASSIS_KNOBS.to_vec()
}

/// The hub's residual knobs: the shared [`chassis_residual_knobs`] plus the
/// hub-only `AETHER_FLEET_STORE_ROOT` hand knob (the engines cap's inline
/// ops override, issue 1968 — a knob with no confique `Meta`, so it stays a
/// hand record rather than an aggregate member).
#[must_use]
pub fn hub_residual_knobs() -> Vec<KnobRecord> {
    let mut records = chassis_residual_knobs();
    records.push(KnobRecord {
        env_key: "AETHER_FLEET_STORE_ROOT",
        doc: "Parent directory for the engines cap's per-engine handle-store dirs; ops escape \
              hatch (unset falls back to the platform data dir, then the system temp dir).",
        default: None,
        kind: KnobKind::HandRegistered,
    });
    records
}

/// The clap `after_help` block every chassis root carries (issue 3882): the
/// "environment-only knobs" — the knobs a chassis resolves but exposes no flag
/// for. [`RuntimeConfig`] (the `AETHER_LOG_FILTER` directive plus the three
/// panic-hook knobs, read by the process-level panic hook directly, below the
/// config layer) and [`FrameSizeConfig`] (`AETHER_MAX_FRAME_SIZE`, pushed into
/// the codec by [`install_frame_size`]) both ride derive-`Config` members whose
/// overlays are deliberately not flattened into the CLI roots. Every other knob
/// a chassis resolves — including the engines cap's `AETHER_FLEET_STORE_ROOT`
/// (`--hub-fleet-store-root`) — has a flag, so it stays out of this section (the
/// issue's no-duplication rule); the residue is the same on all three chassis.
///
/// The per-knob line is harvested from the derive-emitted overlay's clap help,
/// which `aether-derive` builds as `"<summary> [env: KEY] [default: X]"` — the
/// same registry declaration `--print-config` reads, so the section carries the
/// knob's own doc + env + default and can never be a hand-maintained list that
/// drifts. Sorted by env key. Referenced from the CLI roots' `#[command(after_help
/// = …)]`.
#[must_use]
pub fn env_only_after_help() -> String {
    use clap::Args as _;
    use std::fmt::Write as _;

    let mut entries: Vec<(String, String)> = Vec::new();
    for command in [
        RuntimeOverlay::augment_args(clap::Command::new("env-only")),
        FrameSizeOverlay::augment_args(clap::Command::new("env-only")),
    ] {
        for arg in command.get_arguments() {
            let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
            // The env key the derive stamped into the help text (`[env: KEY]`) —
            // the stable sort key, since the leading summary is free prose.
            let env_key = help.split("[env: ").nth(1).and_then(|rest| rest.split(']').next()).unwrap_or_default();
            entries.push((env_key.to_owned(), help));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from(
        "Environment-only knobs (no flag — set via environment, or the matching [section] in \
         --config; --print-config shows their source-resolved values):\n",
    );
    for (_, line) in &entries {
        let _ = writeln!(out, "  {line}");
    }
    out
}

/// A **chassis** config member (ADR-0156 §4): a non-cap knob the chassis
/// resolves at env time and installs onto a dedicated substrate-builder seam —
/// as opposed to a **cap** config member, which rides `with_actor::<Cap>`
/// (builder-resolved, Pass 0). Each impl knows both halves: its own lowering and
/// the builder seam that lowering targets. That is what lets
/// [`BuilderChassisConfigMember::with_chassis_config_member`] install the value
/// and declare the aggregate membership in one fused call, closing the gap where
/// a knob could be declared (and accepted by the unknown-`AETHER_*` sweep) yet
/// silently left at the builder's default because its value seam was forgotten.
pub trait ChassisConfigMember: ConfigMember + 'static {
    /// Lower this resolved member onto its dedicated substrate-builder seam.
    fn install<C: Chassis>(&self, builder: Builder<C>) -> Builder<C>;
}

impl ChassisConfigMember for ChassisBootConfig {
    fn install<C: Chassis>(&self, builder: Builder<C>) -> Builder<C> {
        builder.with_workers(self.to_workers())
    }
}

impl ChassisConfigMember for ActorRingConfig {
    fn install<C: Chassis>(&self, builder: Builder<C>) -> Builder<C> {
        builder.with_ring_capacities(self.to_ring_capacities())
    }
}

impl ChassisConfigMember for SchedulerTuningConfig {
    fn install<C: Chassis>(&self, builder: Builder<C>) -> Builder<C> {
        builder.with_scheduler_tuning(self.to_scheduler_tuning())
    }
}

impl ChassisConfigMember for SettlementConfig {
    fn install<C: Chassis>(&self, builder: Builder<C>) -> Builder<C> {
        builder.with_teardown_budget(self.to_cap())
    }
}

/// Extension seam on the substrate [`Builder`] that fuses a
/// [`ChassisConfigMember`]'s value install with its aggregate-membership
/// declaration. The chassis member types live above `aether-substrate`, so the
/// fused seam cannot be an inherent builder method; this extension trait carries
/// it instead. Every production chassis composes its non-cap members through
/// this call, so the two halves — install the value, declare the membership —
/// can never drift apart.
pub trait BuilderChassisConfigMember<C: Chassis> {
    /// Install the member's value onto its builder seam and declare its
    /// membership on the config aggregate — inseparably, so a declared-and-swept
    /// knob can never be silently left uninstalled.
    #[must_use]
    fn with_chassis_config_member<M: ChassisConfigMember>(self, member: &M) -> Self;
}

impl<C: Chassis> BuilderChassisConfigMember<C> for Builder<C> {
    fn with_chassis_config_member<M: ChassisConfigMember>(self, member: &M) -> Self {
        member.install(self).declare_config_member::<M>()
    }
}

/// Build the single-stage lifecycle params the headless chassis runs
/// (ADR-0082 PR 3b): a `Tick` self-loop with a `Quit` escape to a
/// `Shutdown` terminal. Components subscribe the `Tick` stage directly
/// on `aether.lifecycle` (ADR-0082 §7/§11), so the params wire no
/// initial subscribers. Desktop and `substrate_harness` run the three-stage
/// `Tick → Render → Present` graph from `frame_lifecycle_params()`
/// instead.
///
/// The advance timeout is resolved separately through the
/// [`LifecycleConfig`] `Config` channel.
///
/// # Panics
/// Panics if the (compile-time-fixed) graph fails to build — it can't,
/// the shape is structurally valid; the `expect` documents the
/// invariant.
#[must_use]
pub fn tick_only_lifecycle_params() -> LifecycleParams {
    let graph = LifecycleGraphData::builder()
        .state::<Tick>()
        .next::<Tick>()
        .quit::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Tick>()
        .build()
        .expect("tick-only lifecycle graph is structurally valid");
    LifecycleParams { graph, initial_subscribers: vec![] }
}

/// Args every full-stack chassis hands to [`with_common_caps`]. Kept
/// as a flat struct (no defaults) so an added cap forces the chassis
/// builders to acknowledge it.
///
/// ADR-0156 §5: the operator-resolvable cap `Config`s (`HttpConfig`,
/// `ProcessConfig`, …) no longer ride here — the builder resolves each off the
/// source stack the chassis handed it via `Builder::with_config_sources`. What
/// remains is the composer-supplied construction input (`Params`, the aborter,
/// the resolved non-cap member structs `with_common_caps` fuses onto their
/// builder seams via `with_chassis_config_member`) plus `namespace_roots`,
/// passed programmatically so the fs cap uses the exact roots value the chassis
/// resolved.
pub struct CommonBoot {
    pub aborter: Arc<dyn FatalAborter>,
    /// The resolved boot knobs (worker count + boot manifest). Installed onto the
    /// builder via [`with_chassis_config_member`](BuilderChassisConfigMember::with_chassis_config_member),
    /// which lowers `workers` through [`ChassisBootConfig::to_workers`] onto
    /// `with_workers` and declares the aggregate membership in the same call.
    pub chassis_boot: ChassisBootConfig,
    /// Issue 1990: the resolved per-actor ring-capacity knob. Lowered through
    /// [`ActorRingConfig::to_ring_capacities`] onto `with_ring_capacities` at
    /// install time.
    pub actor_ring: ActorRingConfig,
    /// Issue 2485: the resolved scheduler hot-path tuning knob. Lowered through
    /// [`SchedulerTuningConfig::to_scheduler_tuning`] onto `with_scheduler_tuning`
    /// at install time.
    pub scheduler_tuning: SchedulerTuningConfig,
    /// Issue #2509: the resolved settlement/teardown knob
    /// (`AETHER_SETTLEMENT_CAP_SECS`). Lowered through [`SettlementConfig::to_cap`]
    /// (including its `0 → wait forever` sentinel) onto `with_teardown_budget` at
    /// install time, so one knob covers both the settlement gates and the
    /// teardown gate.
    pub settlement: SettlementConfig,
    /// Composer-supplied wasmtime / egress handles for the component host
    /// cap (ADR-0156 §3 `Params`); the cap's `Config` is `()`.
    pub component_host_params: ComponentHostParams,
    /// The resolved `aether.fs` roots. Resolved chassis-side, then passed to the
    /// fs cap via the builder's programmatic layer so the cap uses the exact
    /// same value.
    pub namespace_roots: NamespaceRoots,
    /// Resolved `TurnSim` wiring for the game gateway (ADR-0156 §3 `Params`).
    /// The default has no configured `TurnSim`, so merely linking the common
    /// chassis opens no player listener.
    pub game_gateway_params: GameGatewayParams,
}

/// The resolved config *data* every full-stack chassis (desktop + headless)
/// carries identically — the single declaration of the shared env, so a knob
/// added here exists on both chassis instead of being hand-copied into one and
/// silently missing from the other. `DesktopEnv` / `HeadlessEnv` embed it as a
/// `common` field and add only their genuinely per-chassis knobs (the desktop
/// window boot knobs, the headless tick cadence) plus their autoload list.
///
/// [`Self::resolve`] is the one shared resolver: it consumes the staged config
/// source stack and produces this struct (the env-side twin of the CLI's
/// `CommonOverlay`). [`Self::into_common_boot`] reads it off into the shared
/// [`CommonBoot`] cap args in one place.
///
/// ADR-0155 §4: this is config only — every field resolves through the
/// argv/env/file path a real boot uses, so `--describe` resolves it on a
/// headless host. A test constructs one by staging programmatic overrides into
/// `sources` (`ConfigSources::set_override`).
pub struct CommonEnv {
    /// ADR-0156 §5: the config source stack (file + per-cap argv overlays) the
    /// builder resolves each composed cap's `Config` off (http / http-server /
    /// audio / render tuning / lifecycle). The cap `Config`s no longer ride as
    /// fields — a test stages programmatic overrides here.
    pub sources: ConfigSources,
    /// The resolved `aether.fs` namespace roots (save / assets / config).
    /// Resolved chassis-side and passed to the fs cap programmatically so the
    /// cap uses the exact same value.
    pub namespace_roots: NamespaceRoots,
    /// The substrate runtime knobs (#3849), resolved off the source stack. Only
    /// [`RuntimeConfig::log_filter`] is consumed chassis-side (re-applied after
    /// the subscriber installs, in each chassis's `Chassis::build`); the field
    /// carries the whole resolved member so its `[runtime]` file / env values are
    /// resolved once. `aether.rpc.server`'s bind port rides the source stack too
    /// (`RpcServerConfig`), so there is no separate `rpc_address` field — the
    /// builder resolves it (unset → claimed but unbound, ADR-0155 §3).
    pub runtime: RuntimeConfig,
    /// Issue 745 / issue #3930: the resolved boot knobs (worker count + boot
    /// manifest). Carried whole so the chassis-member fuse lowers `workers`
    /// through [`ChassisBootConfig::to_workers`] onto the builder's `with_workers`
    /// seam at install time (the headless / desktop boot log line reads the same
    /// lowered value off it). `boot_manifest` is consumed by [`Self::resolve`] for
    /// the autoload list before the struct is stored.
    pub chassis_boot: ChassisBootConfig,
    /// Issue 1990: the resolved per-actor ring-capacity knob
    /// (`AETHER_ACTOR_LOG_RING_SIZE` / `AETHER_ACTOR_TRACE_RING_SIZE`). Lowered to
    /// [`RingCapacities`] through [`ActorRingConfig::to_ring_capacities`] at
    /// install time (and for the boot log line).
    pub actor_ring: ActorRingConfig,
    /// Issue 2485: the resolved scheduler hot-path tuning knob
    /// (`AETHER_SPIN_WINDOW_USEC` / `AETHER_LOCAL_STICKY_MAX` / …). Lowered to
    /// [`SchedulerTuning`] through [`SchedulerTuningConfig::to_scheduler_tuning`]
    /// at install time.
    pub scheduler_tuning: SchedulerTuningConfig,
    /// Issue #2509: the resolved settlement/teardown knob
    /// (`AETHER_SETTLEMENT_CAP_SECS` / `[settlement]`). Lowered to the teardown
    /// budget [`Duration`] through [`SettlementConfig::to_cap`] at install time.
    pub settlement: SettlementConfig,
}

impl CommonEnv {
    /// Resolve the shared chassis config off the already-staged source stack —
    /// the one place the common `AETHER_*` knobs are read, replacing the
    /// per-chassis copies of this block. The caller stages `sources` (config
    /// file + `cli.stage`) and resolves its own per-chassis knob (window / tick)
    /// off the same stack first; this then consumes `sources` into the returned
    /// [`CommonEnv`].
    ///
    /// The boot-manifest autoload list is returned alongside rather than held as
    /// a field: it rides each chassis's own struct (its bring-up is
    /// chassis-flavored), but its production shares the [`ChassisBootConfig`]
    /// resolve with `workers`, so it is resolved here once and handed back.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known `AETHER_*` member (or argv overlay
    /// value) holds an unparseable value (ADR-0090 §4), or when a boot manifest
    /// named by `AETHER_BOOT_MANIFEST` / `--boot-manifest` cannot be read.
    pub fn resolve(mut sources: ConfigSources) -> Result<(Self, Vec<AutoloadComponent>), ConfigError> {
        // Chassis-side reads of resolved members (ADR-0156 §5) — each resolves
        // off the same stack via its `ConfigMember` section: the derived staging
        // inputs (fs roots + content-gen) and the non-cap pool / ring / scheduler
        // / teardown / runtime knobs.
        // #3930: the seam-installed non-cap members ride as resolved structs — the
        // chassis-member fuse (`with_chassis_config_member`) lowers each onto its
        // builder seam at install time, so lowering no longer happens here.
        let chassis_boot = sources.resolve::<ChassisBootConfig>()?;
        let namespace_roots = sources.resolve::<NamespaceRoots>()?;
        let actor_ring = sources.resolve::<ActorRingConfig>()?;
        let scheduler_tuning = sources.resolve::<SchedulerTuningConfig>()?;
        let settlement = sources.resolve::<SettlementConfig>()?;
        // #3849: resolve the substrate runtime knobs (log filter + panic-hook
        // knobs) off the same stack; `Chassis::build` re-applies `log_filter` once
        // the subscriber is installed.
        let runtime = sources.resolve::<RuntimeConfig>()?;
        // ADR-0156 §6 (#3850): push the resolved wire-frame cap into the codec
        // here, before the RPC server binds and any framing runs — the codec
        // cannot pull the knob itself.
        install_frame_size(&mut sources)?;

        // Boot manifest: argv wins over `AETHER_BOOT_MANIFEST` (resolved through
        // `ChassisBootConfig`). When set, the listed components' wasm + config are
        // read into the autoload list `Chassis::build` drains into
        // `aether.component.load`; an unreadable manifest aborts boot (ADR-0090
        // §4) via `ConfigError`.
        let autoload = match chassis_boot.boot_manifest.clone() {
            Some(path) => boot_manifest_autoload(Path::new(&path))?,
            None => Vec::new(),
        };

        Ok((
            Self { sources, namespace_roots, runtime, chassis_boot, actor_ring, scheduler_tuning, settlement },
            autoload,
        ))
    }

    /// Read this resolved env off into the shared [`CommonBoot`] cap args in one
    /// place, so neither chassis's `compose` hand-copies the same env-sourced
    /// fields. The three composer-constructed handles (`aborter`,
    /// `component_host_params`, `game_gateway_params`) are supplied by the caller;
    /// the source stack is threaded back out for the builder's
    /// `with_config_sources`. `runtime` is dropped here — it is consumed
    /// chassis-side (`apply_filter`)
    /// before `compose` runs and is not a `CommonBoot` field.
    #[must_use]
    pub fn into_common_boot(
        self,
        aborter: Arc<dyn FatalAborter>,
        component_host_params: ComponentHostParams,
        game_gateway_params: GameGatewayParams,
    ) -> (CommonBoot, ConfigSources) {
        let Self { sources, namespace_roots, runtime: _, chassis_boot, actor_ring, scheduler_tuning, settlement } =
            self;
        (
            CommonBoot {
                aborter,
                chassis_boot,
                actor_ring,
                scheduler_tuning,
                settlement,
                component_host_params,
                namespace_roots,
                game_gateway_params,
            },
            sources,
        )
    }
}

/// Wire the aborter, worker count, and the common caps every full-
/// stack chassis carries. The renderer / window caps each chassis
/// adds after this in `.with_actor::<_>()` chains.
///
/// ADR-0156 §5: every operator-resolvable cap `Config` is resolved by the
/// builder off the source stack the chassis handed it (`with_config_sources`),
/// so each cap composes with `with_actor::<_>(params)` alone — no per-cap
/// config value, no chassis-side section string. The two exceptions ride the
/// programmatic layer: the fs roots (resolved chassis-side, then passed here so
/// the cap uses the identical value) and the
/// game gateway's `Config`, which stays a hardcoded default so the common
/// chassis opens no player listener from a stray `AETHER_GAME_*` env var
/// (byte-identical to the pre-inversion `GameGatewayConfig::default()`).
///
/// Boot order is declaration order. ADR-0081 retired the central
/// `LogCapability` — every actor owns its own per-actor log ring; no
/// boot ordering is needed for logging anymore.
#[must_use]
pub fn with_common_caps<C: Chassis>(builder: Builder<C>, boot: CommonBoot) -> Builder<C> {
    // ADR-0157 §Security: a subprocess run is confined to the `aether.fs`
    // `save` namespace root — the same writable sandbox the fs cap governs —
    // so it starts inside that sandbox rather than the substrate's cwd. Cloned
    // here because `boot.namespace_roots` is moved into the fs cap below.
    let process_work_root = boot.namespace_roots.save.clone();
    builder
        .with_aborter(boot.aborter)
        // ADR-0156 §4 / #3930: the chassis-declared non-cap members — knobs that
        // configure the shared base rather than any single cap, so they have no
        // `with_actor` entry to ride. Each fuses its value install (`with_workers`
        // / `with_ring_capacities` / `with_scheduler_tuning` / `with_teardown_budget`)
        // with its aggregate-membership declaration in one `with_chassis_config_member`
        // call, so a knob can never be swept-as-known yet silently left at the
        // builder's default.
        .with_chassis_config_member(&boot.chassis_boot)
        .with_chassis_config_member(&boot.actor_ring)
        .with_chassis_config_member(&boot.scheduler_tuning)
        .with_chassis_config_member(&boot.settlement)
        // ADR-0156 §6: the wire-frame-size knob (#3850). It configures the shared
        // codec rather than any single cap, and its value is pushed into the codec
        // by `install_frame_size` at env time — off the builder — so it declares
        // membership only (the retired `AETHER_MAX_FRAME_SIZE` `KnobRecord`'s
        // replacement).
        .declare_config_member::<FrameSizeConfig>()
        // The substrate runtime knobs (log filter + panic-hook knobs, #3849)
        // configure the process, not any single cap, and their value applies off
        // the builder (`apply_filter` in `Chassis::build`, the panic hook's own
        // env read), so they declare membership only.
        .declare_config_member::<RuntimeConfig>()
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<InputCapability>(())
        .with_actor::<ComponentHostCapability>(boot.component_host_params)
        // Programmatic: the fs cap uses the exact roots resolved chassis-side.
        .with_actor_configured::<FsCapability>((), boot.namespace_roots)
        .with_actor::<TextCapability>(())
        .with_actor::<InventoryCapability>(())
        // Builder-resolved off the source stack: `HttpConfig`.
        .with_actor::<HttpCapability>(())
        .with_actor::<TcpCapability>(())
        // ADR-0157: the one-shot exec cap. `ProcessConfig` (deny-by-default
        // allowlist / default timeout / concurrency bound) is builder-resolved
        // off the source stack; the working-directory confinement root rides
        // `Params`, resolved chassis-side from the fs `save` root above.
        .with_actor::<ProcessCapability>(ProcessParams { work_root: process_work_root })
        // Programmatic default (byte-identical to the pre-inversion `::default()`
        // compose): no `AETHER_GAME_*` env opens a listener on the common chassis.
        .with_actor_configured::<GameGatewayCapability>(boot.game_gateway_params, GameGatewayConfig::default())
}

/// This crate's `build.rs`-baked build provenance (ADR-0115): the source
/// revision, build profile, and target triple, read back via `env!`, which
/// resolves in this crate — the one whose build script set them.
///
/// The chassis binaries that depend on `aether-chassis` (desktop / headless /
/// hub) hand this to [`run_describe_prelude`] as their `--describe` provenance.
/// The bloomery chassis, which does not depend on this aggregate, fills a
/// [`BuildProvenance`] from its own crate's `build.rs` instead (ADR-0162).
#[must_use]
pub fn build_provenance() -> BuildProvenance {
    BuildProvenance {
        git_sha: env!("AETHER_GIT_SHA").to_owned(),
        profile: env!("AETHER_BUILD_PROFILE").to_owned(),
        target: env!("AETHER_TARGET_TRIPLE").to_owned(),
    }
}

/// The shared chassis-main prelude for the binaries that depend on
/// `aether-chassis` (desktop / headless / hub): run [`run_chassis_prelude`]
/// with this crate's [`build_provenance`]. A binary calls this straight after
/// parsing its CLI; [`PreludeAction::Handled`] means it printed a discovery
/// dump and `main` should return, [`PreludeAction::Boot`] means it should
/// resolve its env and run (ADR-0162).
///
/// # Errors
///
/// Returns [`BootError`] when env resolution, substrate boot, the claim pass, or
/// manifest serialization fails.
pub fn run_describe_prelude<C: BootableChassis>(flags: PreludeFlags) -> Result<PreludeAction, BootError> {
    run_chassis_prelude::<C>(flags, &build_provenance())
}

/// ADR-0155 §3: always compose the RPC server on the substrate chassis; its
/// `RpcServerConfig.port` (resolved by the builder off the source stack the
/// chassis staged — argv `--rpc-port` > env `AETHER_RPC_PORT` > `[rpc]` file
/// section > default) gates only whether a socket binds. A resolved port
/// starts the listener (substrate becomes an RPC peer a hub or client dials);
/// unset composes the cap disabled — it still claims its `aether.rpc.server`
/// mailbox, so mail to it is answered rather than warn-dropped, and the same
/// binary claims the same namespaces wherever `--describe` runs. The
/// `HelloAck` peer-kind's engine name is derived from `C::PROFILE` by
/// [`aether_substrate::engine_name`], so it is never stated as a second
/// literal beside the profile.
///
/// The config is not staged here (#3849 retired the programmatic-`bind_addr`
/// bridge): the `--rpc-port` overlay `CommonOverlay.rpc` carries rides the CLI
/// root's derived `StageArgv` into the source stack like every other member
/// (issue 3872), and the builder resolves it — unset falls through to the
/// member's `None` default (unbound). Only the hub overrides this with its
/// `DEFAULT_RPC_PORT` fallback, via `with_actor_configured` at its own compose
/// site.
#[must_use]
pub fn with_rpc_server<C: Chassis>(builder: Builder<C>) -> Builder<C> {
    builder.with_actor::<RpcServerCapability>(RpcServerParams {
        peer_kind: PeerKind::Substrate {
            engine_name: aether_substrate::engine_name::<C>(),
            engine_version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![],
        },
        // A forked substrate peer never fields engine-addressed forwards
        // (only the hub does), so it needs no route target.
        route_target: None,
    })
}

#[cfg(test)]
mod tests {
    use super::ActorRingConfig;
    use super::ActorRingConfigLayer;
    use super::ChassisBootConfig;
    use super::SchedulerTuningConfigLayer;
    use super::chassis_residual_knobs;
    use aether_actor::log::DEFAULT_RING_CAP;
    use aether_actor::trace::{DEFAULT_TRACE_RING_CAP, DEFAULT_TRACE_RING_MAX_CAP};
    use aether_lifecycle::{LifecycleConfig, LifecycleConfigLayer};
    use aether_substrate::SchedulerTuning;
    use aether_substrate::config::ConfigError;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::Mutex;
    use std::sync::PoisonError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Process-wide guard around the `AETHER_ACTOR_*` ring env mutation,
    /// so ring tests serialise their set/remove pairs.
    static RING_ENV_LOCK: Mutex<()> = Mutex::new(());
    static CONFIG_FILE_TEST_ID: AtomicUsize = AtomicUsize::new(0);

    fn config_test_path(stem: &str) -> PathBuf {
        let id = CONFIG_FILE_TEST_ID.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!("aether-{stem}-{}-{id}.toml", process::id()))
    }

    #[test]
    fn actor_ring_config_defaults_match() {
        use confique::Config as _;
        // No `.env()` source: literal defaults only — env-free. The
        // layer's `default = 1024 / 4096 / 65536` literals must equal the
        // `aether-actor` const caps so an unset knob reproduces the
        // const-`Default` ring behaviour.
        let _guard = RING_ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let layer = ActorRingConfigLayer::builder().load().expect("defaults load");
        assert_eq!(layer.log_ring_capacity, DEFAULT_RING_CAP);
        assert_eq!(layer.trace_ring_capacity, DEFAULT_TRACE_RING_CAP);
        assert_eq!(layer.trace_ring_max_size, DEFAULT_TRACE_RING_MAX_CAP);
        let default = ActorRingConfig::default();
        assert_eq!(default.log_ring_capacity, DEFAULT_RING_CAP);
        assert_eq!(default.trace_ring_capacity, DEFAULT_TRACE_RING_CAP);
        assert_eq!(default.trace_ring_max_size, DEFAULT_TRACE_RING_MAX_CAP);
        // The default lowers to the same trace floor/ceiling on the `Copy`
        // RingCapacities the spawn path threads.
        let caps = default.to_ring_capacities();
        assert_eq!(caps.trace, DEFAULT_TRACE_RING_CAP);
        assert_eq!(caps.trace_max, DEFAULT_TRACE_RING_MAX_CAP);
    }

    #[test]
    fn actor_ring_config_env_overrides_default() {
        let _guard = RING_ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: serialised by `RING_ENV_LOCK`; set then removed in scope.
        unsafe {
            env::set_var("AETHER_ACTOR_LOG_RING_SIZE", "256");
            env::set_var("AETHER_ACTOR_TRACE_RING_SIZE", "9000");
            env::set_var("AETHER_ACTOR_TRACE_RING_MAX_SIZE", "120000");
        }
        let resolved = ActorRingConfig::from_env().to_ring_capacities();
        // SAFETY: same serialised scope.
        unsafe {
            env::remove_var("AETHER_ACTOR_LOG_RING_SIZE");
            env::remove_var("AETHER_ACTOR_TRACE_RING_SIZE");
            env::remove_var("AETHER_ACTOR_TRACE_RING_MAX_SIZE");
        }
        assert_eq!(resolved.log, 256);
        assert_eq!(resolved.trace, 9000);
        assert_eq!(resolved.trace_max, 120_000);
    }

    #[test]
    fn actor_ring_config_argv_wins_over_env() {
        use confique::Layer as _;
        let _guard = RING_ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: serialised by `RING_ENV_LOCK`.
        unsafe {
            env::set_var("AETHER_ACTOR_TRACE_RING_SIZE", "9000");
        }
        // Argv overlay sets only the trace field; the log field falls
        // through to env (unset) → default. Argv > env > default.
        let mut layer = <ActorRingConfigLayer as confique::Config>::Layer::empty();
        layer.trace_ring_capacity = Some(7777);
        let resolved = ActorRingConfig::from_argv_then_env(layer).to_ring_capacities();
        // SAFETY: same serialised scope.
        unsafe {
            env::remove_var("AETHER_ACTOR_TRACE_RING_SIZE");
        }
        assert_eq!(resolved.trace, 7777, "argv overlay wins over env");
        assert_eq!(resolved.log, DEFAULT_RING_CAP, "unset log falls to default");
    }

    #[test]
    fn scheduler_tuning_defaults_match() {
        use confique::Config as _;
        // Tripwire: the `SchedulerTuningConfigLayer` `default = ...`
        // literals must equal `SchedulerTuning::default()` (issue 2485).
        // The confique layer and the `Copy` `SchedulerTuning` carry the
        // scheduler defaults independently; a change to one and not the
        // other silently shifts the resolved-vs-installed behaviour. No
        // `.env()` source: literal defaults only — env-free.
        let _guard = RING_ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let layer = SchedulerTuningConfigLayer::builder().load().expect("defaults load");
        let default = SchedulerTuning::default();
        assert_eq!(layer.spin_window_micros, default.spin_window_micros);
        assert_eq!(layer.local_sticky_max, default.local_sticky_max);
        assert_eq!(layer.peer_steal, default.peer_steal);
        assert_eq!(layer.local_chain_backstop, default.local_chain_backstop);
        assert_eq!(layer.blob_recruit_min, default.blob_recruit_min);
        assert_eq!(layer.blob_recruit_max, default.blob_recruit_max);
        // The three adaptive knobs default unset (None → measured/derived).
        assert_eq!(layer.time_budget_micros, None);
        assert_eq!(layer.handoff_cost_nanos, None);
        assert_eq!(layer.wake_cost_nanos, None);
    }

    #[test]
    fn to_workers_none_returns_none() {
        // No workers knob set — pool uses PoolConfig::default.
        assert_eq!(ChassisBootConfig::default().to_workers(), None);
    }

    #[test]
    fn to_workers_positive_returns_some() {
        // Positive value passes through unchanged.
        let cfg = ChassisBootConfig { workers: Some(4), ..ChassisBootConfig::default() };
        assert_eq!(cfg.to_workers(), Some(4));
    }

    #[test]
    fn to_workers_zero_clamps_to_one() {
        // The 0→1 clamp: the only real logic this crate owns for the workers knob.
        let cfg = ChassisBootConfig { workers: Some(0), ..ChassisBootConfig::default() };
        assert_eq!(cfg.to_workers(), Some(1));
    }

    #[test]
    fn residual_knobs_are_the_config_file_meta_only() {
        // Tripwire (#3849 + #3850): after the RPC-port, runtime, and frame-size
        // knobs migrated onto derive-`Config` members, the residual set shrank to
        // exactly one record the composition-derived aggregate can't own — the
        // `AETHER_CONFIG_FILE` meta-knob (it selects the file source, so it
        // cannot live in the file). Every migrated key must NOT reappear here, or
        // it would be double-registered (member + residual).
        let keys: Vec<&str> = chassis_residual_knobs().iter().map(|record| record.env_key).collect();
        assert_eq!(keys, ["AETHER_CONFIG_FILE"], "the config-file meta-knob is the sole residual survivor");
        for migrated in [
            "AETHER_RPC_PORT",
            "AETHER_MAX_FRAME_SIZE",
            "AETHER_LOG_FILTER",
            "AETHER_BACKTRACE",
            "AETHER_CRASH_LOG_DISABLE",
            "AETHER_CRASH_LOG_DIR",
        ] {
            assert!(
                !keys.contains(&migrated),
                "{migrated} moved onto a derive-Config member — it must not stay residual"
            );
        }
    }

    #[test]
    fn hub_residual_knobs_add_the_fleet_store_root() {
        // The hub folds one extra hand record over the shared residual set:
        // the engines-cap `AETHER_FLEET_STORE_ROOT` ops override (a knob with
        // no confique `Meta`, so it stays a hand record rather than a member).
        let keys: Vec<&str> = super::hub_residual_knobs().iter().map(|record| record.env_key).collect();
        assert!(keys.contains(&"AETHER_FLEET_STORE_ROOT"), "hub residual knobs must carry AETHER_FLEET_STORE_ROOT");
        assert!(keys.contains(&"AETHER_CONFIG_FILE"), "hub residual knobs must extend the shared residual set");
    }

    #[test]
    fn lifecycle_advance_timeout_default_matches_settlement_const() {
        use confique::Config as _;
        // Tripwire: the `LifecycleConfigLayer` `default = 1000` literal must
        // equal the settlement-owned `ADVANCE_TIMEOUT_MS_DEFAULT` const
        // (surfaced as `LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT`), so an
        // unset knob reproduces the cap's const default. Drifts when either the
        // const or the derive literal changes. ADR-0156 §3 relocated this knob
        // off `ChassisBootConfig` onto the lifecycle cap's own config, so the
        // guard moves here with it. No `.env()` source: literal defaults only —
        // env-free.
        let _guard = RING_ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let layer = LifecycleConfigLayer::builder().load().expect("defaults load");
        assert_eq!(layer.advance_timeout_millis, LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT);
    }

    #[test]
    fn load_config_file_errors_on_missing_or_malformed_file() {
        let missing = config_test_path("missing-config");
        assert!(
            matches!(super::load_config_file(&missing), Err(ConfigError::ConfigFile { .. })),
            "explicit missing config file must hard-error",
        );

        let malformed = config_test_path("malformed-config");
        fs::write(&malformed, "[http\n").expect("write malformed config");
        let result = super::load_config_file(&malformed);
        let _ = fs::remove_file(&malformed);
        assert!(matches!(result, Err(ConfigError::ConfigFile { .. })), "malformed config file must hard-error");
    }
}
