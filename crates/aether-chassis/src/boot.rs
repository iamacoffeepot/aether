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

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aether_actor::log::DEFAULT_RING_CAP;
use aether_actor::trace::{DEFAULT_TRACE_RING_CAP, DEFAULT_TRACE_RING_MAX_CAP};
use aether_anthropic::{AnthropicCapability, AnthropicConfig};
use aether_audio::AudioConfig;
use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_contentgen::ContentGenConfig;
use aether_fs::{FsCapability, NamespaceRoots};
use aether_game::{GameGatewayCapability, GameGatewayConfig, GameGatewayParams};
use aether_gemini::{GeminiCapability, GeminiConfig, GeminiParams};
use aether_http::{HttpCapability, HttpConfig, HttpServerConfig};
use aether_input::InputCapability;
use aether_inventory::InventoryCapability;
use aether_kinds::{BinaryManifest, Shutdown, Tick};
use aether_lifecycle::{LifecycleConfig, LifecycleGraphData, LifecycleParams};
use aether_render::RenderTuningConfig;
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig, RpcServerParams};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::config::{ConfigError, FromArgvThenEnv, KnobKind, KnobRecord, RingCapacities, SchedulerTuning};
use aether_substrate::runtime::RUNTIME_KNOBS;
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_tcp::TcpCapability;
use aether_text::TextCapability;
use aether_trace::TraceDispatchCapability;

use crate::tick::TickConfig;
use crate::window::WindowConfig;

/// Env fallback for the chassis config-file path. The path is
/// meta-config: it selects the file source and does not change the file
/// layer's lower precedence relative to ordinary `AETHER_*` env knobs.
pub const CONFIG_FILE_ENV: &str = "AETHER_CONFIG_FILE";

/// Chassis-direct env knobs that aren't `#[derive(Config)]` fields —
/// the hand-registered knobs the chassis bins read inline
/// (`AETHER_RPC_PORT`) plus the one orphaned codec knob that can't
/// live substrate-side (`AETHER_MAX_FRAME_SIZE` — `aether-codec`
/// cannot depend on `aether-substrate`, where `KnobRecord`/`KnobKind`
/// live). Registered as [`KnobRecord`]s so e1's unknown-`AETHER_*`
/// sweep doesn't flag them and e2's `--print-config` dump lists them.
/// ADR-0090 §1/§4. The runtime (log / panic-hook) knobs are registered
/// by `aether_substrate::runtime::RUNTIME_KNOBS`; the scheduler
/// hot-path, chassis boot, window, and tick knobs are covered by the
/// derive-emitted `*Layer::META`s in `chassis_registry`.
pub const CHASSIS_KNOBS: &[KnobRecord] = &[
    KnobRecord {
        env_key: CONFIG_FILE_ENV,
        doc: "Path to a sectioned TOML chassis config file; overridden by --config <PATH>.",
        default: None,
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_RPC_PORT",
        doc: "aether.rpc.server bind port; desktop/headless skip the server when unset, hub always \
              binds (default 8901).",
        default: None,
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_MAX_FRAME_SIZE",
        doc: "Maximum accepted wire-frame body size in bytes (see \
              aether_codec::frame::max_frame_size); default mirrors \
              aether_codec::frame::MAX_FRAME_SIZE (64 MiB) — keep the two in sync.",
        default: Some("67108864"),
        kind: KnobKind::HandRegistered,
    },
];

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

/// Extract one `[section]` from the parsed chassis config file and
/// deserialize it into the target config's partial confique layer. An
/// absent section is `None`; a present but malformed section is a hard
/// boot error.
///
/// # Errors
///
/// Returns [`ConfigError`] when a present section is not a table or
/// cannot deserialize into the target layer.
pub fn file_section<C: FromArgvThenEnv>(
    table: &toml::Table,
    section: &str,
) -> Result<Option<<C::Layer as confique::Config>::Layer>, ConfigError> {
    let Some(value) = table.get(section) else {
        return Ok(None);
    };
    if !matches!(value, toml::Value::Table(_)) {
        let source = io::Error::new(io::ErrorKind::InvalidData, format!("expected [{section}] to be a TOML table"));
        return Err(ConfigError::config_section(section, source));
    }
    value.clone().try_into().map(Some).map_err(|source| ConfigError::config_section(section, source))
}

/// Resolve a config from argv/env plus the optional file section named
/// by `section`, preserving source priority argv > env > file > defaults.
///
/// # Errors
///
/// Returns [`ConfigError`] from section deserialization or config
/// resolution.
pub fn resolve_with_file<C: FromArgvThenEnv>(
    argv: <C::Layer as confique::Config>::Layer,
    file: Option<&toml::Table>,
    section: &str,
) -> Result<C, ConfigError> {
    let file = file.map(|table| file_section::<C>(table, section)).transpose()?.flatten();
    C::try_resolve(argv, file)
}

/// Resolve a config with no argv overlay, but with the optional file
/// section below env.
///
/// # Errors
///
/// Returns [`ConfigError`] from section deserialization or config
/// resolution.
pub fn resolve_env_with_file<C: FromArgvThenEnv>(file: Option<&toml::Table>, section: &str) -> Result<C, ConfigError> {
    let argv = <<C::Layer as confique::Config>::Layer as confique::Layer>::empty();
    resolve_with_file::<C>(argv, file, section)
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
    /// `AETHER_ACTOR_LOG_RING_SIZE=<entries>` per-actor log-ring capacity
    /// (default [`DEFAULT_RING_CAP`]). Zero clamps to 1 inside
    /// `ActorLogRing::with_capacity`.
    #[config(env = "AETHER_ACTOR_LOG_RING_SIZE", default = 1024)]
    pub log_ring_capacity: usize,
    /// `AETHER_ACTOR_TRACE_RING_SIZE=<entries>` per-actor (and
    /// chassis-host) trace-ring *floor* — the size each ring starts at
    /// (default [`DEFAULT_TRACE_RING_CAP`]). Zero clamps to 1 inside
    /// `ActorTraceRing::with_growth`.
    #[config(env = "AETHER_ACTOR_TRACE_RING_SIZE", default = 4096)]
    pub trace_ring_capacity: usize,
    /// `AETHER_ACTOR_TRACE_RING_MAX_SIZE=<entries>` ceiling a saturating
    /// trace ring grows to before it resumes drop-oldest (default
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
    /// `AETHER_SPIN_WINDOW_USEC=<micros>` route-to-spinner spin-window
    /// before a worker parks (default `50`). A `0` is valid (no spin).
    #[config(env = "AETHER_SPIN_WINDOW_USEC", default = 50)]
    pub spin_window_micros: u64,
    /// `AETHER_LOCAL_STICKY_MAX=<slots>` deque-length backstop (default
    /// `256`; a resolved `0` coerces to the default).
    #[config(env = "AETHER_LOCAL_STICKY_MAX", default = 256, nonzero)]
    pub local_sticky_max: usize,
    /// `AETHER_LOCAL_TIME_BUDGET_US=<micros>` keep-local time valve.
    /// Unset → adaptive (derived from the measured handoff cost); `0`
    /// disables the valve (pure inline-cascade).
    #[config(env = "AETHER_LOCAL_TIME_BUDGET_US")]
    pub time_budget_micros: Option<u64>,
    /// `AETHER_PEER_STEAL=<bool>` whether idle workers may raid siblings'
    /// deques (default `false` — owner-only). Accepts `1`/`true`/`yes`.
    #[config(env = "AETHER_PEER_STEAL", default = false)]
    pub peer_steal: bool,
    /// `AETHER_LOCAL_CHAIN_BACKSTOP=<k>` every-K injector backstop for
    /// keep-local chains (default `64`; a resolved `0` coerces to it).
    #[config(env = "AETHER_LOCAL_CHAIN_BACKSTOP", default = 64, nonzero)]
    pub local_chain_backstop: u32,
    /// `AETHER_HANDOFF_COST_NS=<nanos>` pins the cross-worker handoff-cost
    /// estimate and freezes live refinement. Unset / `< 1` → boot-probed
    /// and live-refined ([`Self::to_scheduler_tuning`] filters `< 1` to
    /// `None`).
    #[config(env = "AETHER_HANDOFF_COST_NS")]
    pub handoff_cost_nanos: Option<u64>,
    /// `AETHER_BLOB_RECRUIT_MIN=<groups>` minimum fresh-group count for a
    /// flush to broadcast-recruit siblings (default `9`; `0` coerces to
    /// it).
    #[config(env = "AETHER_BLOB_RECRUIT_MIN", default = 9, nonzero)]
    pub blob_recruit_min: usize,
    /// `AETHER_BLOB_RECRUIT_MAX=<copies>` cap on sibling copies a single
    /// flush injects when recruiting (default `32`; `0` coerces to it).
    #[config(env = "AETHER_BLOB_RECRUIT_MAX", default = 32, nonzero)]
    pub blob_recruit_max: usize,
    /// `AETHER_WAKE_COST_NANOS=<nanos>` pins the recruit wake break-even
    /// and freezes live refinement. Unset / `< 1` → the box-measured
    /// handoff cost ([`Self::to_scheduler_tuning`] filters `< 1` to
    /// `None`).
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
    /// `AETHER_SUBSTRATE_HARNESS_SIZE=WxH` render dimensions for the offscreen
    /// wgpu surface. Falls back to `800x600` on missing/unparseable input
    /// with a warn log (default `None`).
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
/// cumulative-patience cap from the shared `AETHER_SETTLEMENT_CAP_SECS`
/// knob (`SettlementConfig::to_cap`, including its `0 → Duration::MAX`
/// "wait forever" sentinel). Each chassis threads the result into the
/// substrate `Builder` via `with_teardown_cap`, so one knob covers both
/// the settlement gates and the teardown gate. A crate-root re-export
/// keeps it reachable from the chassis bins (which cannot see the private
/// `SettlementConfig`).
#[must_use]
pub fn resolve_teardown_cap() -> Duration {
    SettlementConfig::from_env().to_cap()
}

/// Resolve the teardown cap from argv/env plus the optional
/// `[settlement]` config-file section.
///
/// # Errors
///
/// Returns [`ConfigError`] when the file section or env value is
/// malformed.
pub fn resolve_teardown_cap_with_file(file: Option<&toml::Table>) -> Result<Duration, ConfigError> {
    Ok(resolve_env_with_file::<SettlementConfig>(file, "settlement")?.to_cap())
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
    /// `AETHER_WORKERS=<n>` worker-pool size override (unset →
    /// `available_parallelism()-1`, min 1). `Option<usize>` soft-parses
    /// (unparseable → `None`, matching the old `parse_workers_env`
    /// fallback). The 0→1 clamp logic lives in [`Self::to_workers`].
    #[config(cli_long = "workers")]
    pub workers: Option<usize>,
    /// `AETHER_BOOT_MANIFEST=<path>` path to a `BundleManifest` JSON
    /// of components to auto-load at boot (the runtime twin of the
    /// standalone-bundle compile-time pack; injected by the engines cap
    /// on a `spawn_substrate` carrying components). `Option<String>`
    /// filters empty → `None`, exactly matching `boot_manifest_from_env`.
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

/// The residual hand-registered knobs the composition-derived
/// [`ConfigManifest`](aether_substrate::config::ConfigManifest) doesn't yet
/// own (ADR-0156 §6 folds these in on later slices): the chassis-direct
/// records ([`CHASSIS_KNOBS`] — the config-file path, RPC port, orphaned
/// frame-size knob) plus the runtime log-filter / panic-hook knobs
/// (`aether_substrate::runtime::RUNTIME_KNOBS`). Folded in beside the
/// manifest metas by every chassis's known-keys sweep and `--print-config`
/// dump ([`ConfigManifest::known_keys`](aether_substrate::config::ConfigManifest::known_keys)
/// / [`dump`](aether_substrate::config::ConfigManifest::dump)).
#[must_use]
pub fn chassis_residual_knobs() -> Vec<KnobRecord> {
    let mut records: Vec<KnobRecord> = CHASSIS_KNOBS.to_vec();
    records.extend_from_slice(RUNTIME_KNOBS);
    records
}

/// The hub's residual knobs: the shared [`chassis_residual_knobs`] plus the
/// hub-only `AETHER_ENGINE_STORE_ROOT` hand knob (the engines cap's inline
/// ops override, issue 1968 — a knob with no confique `Meta`, so it stays a
/// hand record rather than an aggregate member).
#[must_use]
pub fn hub_residual_knobs() -> Vec<KnobRecord> {
    let mut records = chassis_residual_knobs();
    records.push(KnobRecord {
        env_key: "AETHER_ENGINE_STORE_ROOT",
        doc: "Parent directory for the engines cap's per-engine handle-store dirs; ops escape \
              hatch (unset falls back to the platform data dir, then the system temp dir).",
        default: None,
        kind: KnobKind::HandRegistered,
    });
    records
}

/// Declare the hub's fleet pass-through config members (ADR-0156 §4) — the
/// **one documented over-approximation** in the aggregate. The hub composes
/// only its own thin cap set (trace dispatcher, engines cap, RPC server), but
/// a hub-spawned substrate inherits the hub's process environment, so the
/// full fleet cap knobs legitimately sit in the hub's env destined for the
/// spawned engine. Rather than derive that set from a composition the hub
/// never runs, the hub declares it explicitly here: every operator-resolvable
/// cap `Config` a full-stack substrate composes, plus the chassis-declared
/// non-cap members. This is deliberately a superset of what the hub itself
/// wires — the union of every substrate profile's knobs — so the hub's
/// unknown-`AETHER_*` sweep never flags a fleet knob an operator legitimately
/// sets for the substrates it spawns.
#[must_use]
pub fn with_hub_fleet_passthrough<C: Chassis>(builder: Builder<C>) -> Builder<C> {
    builder
        .with_config_member::<HttpConfig>()
        .with_config_member::<HttpServerConfig>()
        .with_config_member::<GeminiConfig>()
        .with_config_member::<ContentGenConfig>()
        .with_config_member::<AnthropicConfig>()
        .with_config_member::<AudioConfig>()
        .with_config_member::<NamespaceRoots>()
        .with_config_member::<RenderTuningConfig>()
        .with_config_member::<LifecycleConfig>()
        .with_config_member::<GameGatewayConfig>()
        .with_config_member::<WindowConfig>()
        .with_config_member::<TickConfig>()
        .with_config_member::<ChassisBootConfig>()
        .with_config_member::<ActorRingConfig>()
        .with_config_member::<SchedulerTuningConfig>()
        .with_config_member::<SettlementConfig>()
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
/// [`LifecycleConfig`](aether_lifecycle::LifecycleConfig) `Config` channel.
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
pub struct CommonBoot {
    pub aborter: Arc<dyn FatalAborter>,
    pub workers: Option<usize>,
    /// Issue 1990: per-actor ring capacities, resolved from the
    /// `ActorRingConfig` derive-`Config` knob in the chassis main.
    pub ring_caps: RingCapacities,
    /// Issue 2485: scheduler hot-path tuning, resolved from the
    /// `SchedulerTuningConfig` derive-`Config` knob in the chassis main.
    pub scheduler_tuning: SchedulerTuning,
    /// Issue #2509: cumulative patience for the instanced-actor teardown
    /// close-done gate, resolved from the same `SettlementConfig`
    /// (`AETHER_SETTLEMENT_CAP_SECS`) knob the settlement gates read (via
    /// [`SettlementConfig::to_cap`]), so one knob covers both. Threaded
    /// into the `Builder` via `with_teardown_cap`.
    pub teardown_cap: Duration,
    /// Composer-supplied wasmtime / egress handles for the component host
    /// cap (ADR-0156 §3 `Params`); the cap's `Config` is `()`.
    pub component_host_params: ComponentHostParams,
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConfig,
    pub anthropic: AnthropicConfig,
    pub gemini: GeminiConfig,
    /// Content-gen staging config (ADR-0090). `with_common_caps` folds
    /// its `gen_dir` override (else the resolved `save`-namespace root)
    /// into the staging root threaded into the gemini cap.
    pub contentgen: ContentGenConfig,
    /// Operator-typable game-gateway listener/session config (ADR-0090).
    pub game_gateway: GameGatewayConfig,
    /// Resolved `TurnSim` wiring for the game gateway (ADR-0156 §3 `Params`).
    /// The default has no configured `TurnSim`, so merely linking the common
    /// chassis opens no player listener.
    pub game_gateway_params: GameGatewayParams,
}

/// Wire the aborter, worker count, and the common caps every full-
/// stack chassis carries. The renderer / window caps each chassis
/// adds after this in `.with_actor::<_>()` chains.
///
/// Boot order is declaration order. ADR-0081 retired the central
/// `LogCapability` — every actor owns its own per-actor log ring; no
/// boot ordering is needed for logging anymore.
#[must_use]
pub fn with_common_caps<C: Chassis>(builder: Builder<C>, boot: CommonBoot) -> Builder<C> {
    // Resolve the content-gen staging root once, here, where the resolved
    // `NamespaceRoots.save` is in scope: the `AETHER_GEN_DIR` override wins,
    // else staging tracks the `save`-namespace root the fs cap already owns
    // (preserving its `AETHER_SAVE_DIR` → platform fallback without re-reading
    // env). Threaded into the gemini cap via `GeminiParams`.
    let staging_root = boot.contentgen.gen_dir.clone().unwrap_or_else(|| boot.namespace_roots.save.clone());
    builder
        .with_aborter(boot.aborter)
        .with_workers(boot.workers)
        .with_ring_caps(boot.ring_caps)
        .with_scheduler_tuning(boot.scheduler_tuning)
        .with_teardown_cap(boot.teardown_cap)
        // ADR-0156 §4: the chassis-declared non-cap members — knobs that
        // configure the shared base rather than any single cap, so they ride
        // the dedicated builder seams above (`with_workers` / `with_ring_caps`
        // / `with_scheduler_tuning` / `with_teardown_cap`) instead of a
        // `with_actor` entry, and declare their aggregate membership here.
        // `ContentGenConfig` joins them: its `AETHER_GEN_DIR` staging knob is
        // folded into `GeminiParams::gen_root` below rather than composed as a
        // cap, so it too has no `with_actor` entry to ride.
        .with_config_member::<ChassisBootConfig>()
        .with_config_member::<ActorRingConfig>()
        .with_config_member::<SchedulerTuningConfig>()
        .with_config_member::<SettlementConfig>()
        .with_config_member::<ContentGenConfig>()
        .with_actor::<TraceDispatchCapability>((), ())
        .with_actor::<InputCapability>((), ())
        .with_actor::<ComponentHostCapability>((), boot.component_host_params)
        .with_actor::<FsCapability>(boot.namespace_roots, ())
        .with_actor::<TextCapability>((), ())
        .with_actor::<InventoryCapability>((), ())
        .with_actor::<HttpCapability>(boot.http, ())
        .with_actor::<TcpCapability>((), ())
        .with_actor::<GameGatewayCapability>(boot.game_gateway, boot.game_gateway_params)
        .with_actor::<AnthropicCapability>(boot.anthropic, ())
        .with_actor::<GeminiCapability>(boot.gemini, GeminiParams { gen_root: staging_root })
}

/// Assemble a chassis bin's `--describe` [`BinaryManifest`] (ADR-0115,
/// amended by ADR-0155): the chassis profile, the mailbox namespaces it
/// claims, and the build provenance `build.rs` baked into this crate
/// (`AETHER_GIT_SHA` / `AETHER_BUILD_PROFILE` / `AETHER_TARGET_TRIPLE`).
/// The `env!`s resolve in this crate, where `build.rs` set them.
///
/// `caps` is the claim-derived namespace set the describe path reads off
/// `Builder::claim_namespaces` — the same claim code a real boot runs over
/// the composed `with_actor` chain, driver claims, and inline sinks — so
/// there is no hand-maintained list to drift. The [`BTreeSet`] arrives
/// sorted; the manifest's `caps` field preserves that order. Each chassis
/// bin calls this on `--describe`, prints the JSON, and exits before boot
/// — the hub's binary store forks `<binary> --describe` once at upload time
/// to capture exactly this.
#[must_use]
pub fn binary_manifest(chassis: &str, caps: BTreeSet<String>) -> BinaryManifest {
    BinaryManifest {
        chassis: chassis.to_owned(),
        caps: caps.into_iter().collect(),
        git_sha: env!("AETHER_GIT_SHA").to_owned(),
        profile: env!("AETHER_BUILD_PROFILE").to_owned(),
        target: env!("AETHER_TARGET_TRIPLE").to_owned(),
    }
}

/// ADR-0155 §3: always compose the RPC server on the substrate chassis;
/// the resolved `rpc_addr` gates only whether a socket binds. `Some(addr)`
/// starts the listener (substrate becomes an RPC peer a hub or client
/// dials); `None` composes the cap disabled — it still claims its
/// `aether.rpc.server` mailbox, so mail to it is answered rather than
/// warn-dropped, and the same binary claims the same namespaces wherever
/// `--describe` runs. `engine_name` identifies the chassis profile in the
/// `HelloAck` peer-kind.
#[must_use]
pub fn with_rpc_server<C: Chassis>(builder: Builder<C>, rpc_addr: Option<SocketAddr>, engine_name: &str) -> Builder<C> {
    builder.with_actor::<RpcServerCapability>(
        RpcServerConfig { bind_addr: rpc_addr.map(|addr| addr.to_string()) },
        RpcServerParams {
            peer_kind: PeerKind::Substrate {
                engine_name: engine_name.into(),
                engine_version: env!("CARGO_PKG_VERSION").into(),
                kinds: vec![],
            },
            // A forked substrate peer never fields engine-addressed forwards
            // (only the hub does), so it needs no route target.
            route_target: None,
        },
    )
}

/// Parse the `AETHER_RPC_PORT` env var into an optional port number
/// (issue 792). `None` when unset or unparseable. The hub chassis
/// substitutes its default port when this returns `None`; the desktop
/// and headless chassis treat `None` as "don't boot the RPC server"
/// instead.
#[must_use]
// Chassis boot config: the AETHER_RPC_PORT fallback for an absent --rpc-port flag
// (the hub injects this into forked engines), read at the process boundary — not
// a cap config knob.
#[allow(clippy::disallowed_methods)]
pub fn rpc_port_from_env() -> Option<u16> {
    env::var("AETHER_RPC_PORT").ok().and_then(|s| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::ActorRingConfig;
    use super::ActorRingConfigLayer;
    use super::ChassisBootConfig;
    use super::SchedulerTuningConfigLayer;
    use super::chassis_residual_knobs;
    use super::file_section;
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
    fn residual_knobs_carry_the_infra_records() {
        // Tripwire: the composition-derived aggregate doesn't yet own the
        // chassis-direct records (config-file path, RPC port, frame size) or
        // the runtime log/panic knobs — they ride `chassis_residual_knobs`,
        // folded into every chassis's known-keys sweep beside the manifest
        // metas (ADR-0156 §6 relocates them onto members on later slices).
        // Catches a refactor that drops the `RUNTIME_KNOBS` extend or a
        // `CHASSIS_KNOBS` record, re-introducing a false unknown-key warning.
        let keys: Vec<&str> = chassis_residual_knobs().iter().map(|record| record.env_key).collect();
        for key in [
            "AETHER_CONFIG_FILE",
            "AETHER_RPC_PORT",
            "AETHER_MAX_FRAME_SIZE",
            "AETHER_LOG_FILTER",
            "AETHER_BACKTRACE",
            "AETHER_CRASH_LOG_DISABLE",
            "AETHER_CRASH_LOG_DIR",
        ] {
            assert!(keys.contains(&key), "chassis_residual_knobs missing {key}");
        }
    }

    #[test]
    fn hub_residual_knobs_add_the_engine_store_root() {
        // The hub folds one extra hand record over the shared residual set:
        // the engines-cap `AETHER_ENGINE_STORE_ROOT` ops override (a knob with
        // no confique `Meta`, so it stays a hand record rather than a member).
        let keys: Vec<&str> = super::hub_residual_knobs().iter().map(|record| record.env_key).collect();
        assert!(keys.contains(&"AETHER_ENGINE_STORE_ROOT"), "hub residual knobs must carry AETHER_ENGINE_STORE_ROOT");
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

    #[test]
    fn file_section_absent_is_none_and_present_section_deserializes() {
        let table = "[http]\ndisabled = true\n".parse::<toml::Table>().expect("parse table");
        assert!(
            file_section::<ActorRingConfig>(&table, "actor").expect("absent section ok").is_none(),
            "absent sections fall through to env/defaults",
        );

        let table = "[actor]\nlog_ring_capacity = 2048\n".parse::<toml::Table>().expect("parse table");
        let section = file_section::<ActorRingConfig>(&table, "actor")
            .expect("present section decodes")
            .expect("actor section present");
        assert_eq!(section.log_ring_capacity, Some(2048));
    }
}
