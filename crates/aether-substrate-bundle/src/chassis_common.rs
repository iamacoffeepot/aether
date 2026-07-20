//! Shared `Builder` boot fragments for the desktop and headless
//! chassis. Both `Chassis::build` impls pre-extraction wired the same
//! 10-cap base (handle, log, trace, input, component-host, fs, http,
//! tcp + the aborter + worker count) and the same optional RPC
//! server tail, with only their renderer + window stack differing.
//! The duplicate-code check flagged the parallel chains as duplicated code; this module
//! pulls the shared scaffolding out so each chassis declares only
//! the parts that genuinely differ.
//!
//! The hub and substrate-bench chassis don't share this base (hub is a
//! minimal RPC-only chassis, substrate-bench drives a loopback), so the
//! helper module stays scoped to the two full-stack chassis.

use std::env;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aether_actor::Addressable;
use aether_actor::log::DEFAULT_RING_CAP;
use aether_actor::trace::{DEFAULT_TRACE_RING_CAP, DEFAULT_TRACE_RING_MAX_CAP};
use aether_anthropic::{AnthropicCapability, AnthropicConfig, AnthropicConfigLayer};
use aether_audio::AudioConfigLayer;
use aether_component::{ComponentHostCapability, ComponentHostConfig};
use aether_contentgen::{ContentGenConfig, ContentGenConfigLayer};
use aether_engine::EngineConfigLayer;
use aether_fs::{FsCapability, NamespaceRoots, NamespaceRootsLayer};
use aether_game::{GameGatewayCapability, GameGatewayConfig};
use aether_gemini::{GeminiBoot, GeminiCapability, GeminiConfig, GeminiConfigLayer};
use aether_http::HttpConfigLayer;
use aether_http::HttpServerConfigLayer;
use aether_http::{HttpCapability, HttpConfig, HttpServerCapability, HttpServerConfig};
use aether_input::{InputCapability, InputConfig};
use aether_inventory::InventoryCapability;
use aether_kinds::{BinaryManifest, Present, Render, Shutdown, Tick};
use aether_lifecycle::{LifecycleConfig, LifecycleGraphData};
use aether_render::RenderTuningConfigLayer;
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::config::{
    ConfigError, FromArgvThenEnv, KnobKind, KnobRecord, KnownKeys, RingCapacities, SchedulerTuning, dump_config,
    known_keys,
};
use aether_substrate::runtime::RUNTIME_KNOBS;
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_tcp::TcpCapability;
use aether_text::TextCapability;
use aether_trace::TraceDispatchCapability;
use confique::Config as _;
use confique::meta::Meta;

use crate::desktop::driver::WindowConfigLayer;
use crate::headless::driver::TickConfigLayer;

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
/// derive-emitted `*Layer::META`s in [`chassis_registry`].
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

/// Default cumulative settlement-patience cap, in seconds (issue 2062).
/// Five minutes — a generous deadlock/livelock backstop a healthy chain
/// never reaches even on a saturated box, not the gate a healthy chain
/// meets. The literal `default = 300` on [`SettlementConfig`] must equal
/// this; `settlement_config_defaults_match` guards the pair.
const DEFAULT_SETTLEMENT_CAP_SECS: u64 = 300;

/// Settlement-patience backstop knob (issue 2062). The bench's settlement
/// gates block on the settlement signal and treat this cap as a generous
/// deadlock/livelock backstop, not the 30 s wall-clock correctness gate
/// that false-fired under `nextest --workspace` saturation (a healthy-but-
/// slow chain settling at e.g. 45 s was wrongly declared wedged). The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `SettlementConfigLayer`, the clap-shaped `SettlementOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims (ADR-0090 unit g) — mirrors
/// [`ActorRingConfig`]. Resolved once at gate construction and lowered via
/// [`Self::to_cap`] to the `Duration` the bench reads.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_SETTLEMENT", cli_prefix = "settlement")]
pub struct SettlementConfig {
    /// `AETHER_SETTLEMENT_CAP_SECS=<seconds>` cumulative settlement
    /// patience before a gate is declared wedged (default
    /// [`DEFAULT_SETTLEMENT_CAP_SECS`]). `0` is the sentinel for "no cap —
    /// wait forever," for attaching a debugger to a suspected deadlock; in
    /// that mode the per-round warn log stays the live signal.
    #[config(env = "AETHER_SETTLEMENT_CAP_SECS", default = 300)]
    pub cap_secs: u64,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        Self { cap_secs: DEFAULT_SETTLEMENT_CAP_SECS }
    }
}

impl SettlementConfig {
    /// Lower the resolved knob to the cumulative-cap [`Duration`] the
    /// settlement gates read. `0` maps to [`Duration::MAX`] — the
    /// "no cap" sentinel, which the gate's `waited >= cap` test never
    /// trips, so the wait blocks on the signal forever.
    #[must_use]
    pub fn to_cap(&self) -> Duration {
        if self.cap_secs == 0 {
            Duration::MAX
        } else {
            Duration::from_secs(self.cap_secs)
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

/// Default lifecycle advance timeout in milliseconds. The literal
/// `default = 1000` on [`ChassisBootConfig`] must equal this;
/// `chassis_boot_config_defaults_match` guards the pair.
const DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS: u64 = 1_000;

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
#[derive(Clone, Debug, aether_substrate::Config)]
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
    /// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS=<ms>` force-complete deadline
    /// (ms) for a pending lifecycle advance's `Settled` (issue 1048,
    /// ADR-0082). Default [`DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS`] (1 s).
    /// A garbage value hard-errors at boot (ADR-0090 §4 strict path),
    /// replacing the old soft-warn fallback.
    #[config(
        env = "AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS",
        cli_long = "lifecycle-advance-timeout-millis",
        default = 1000
    )]
    pub lifecycle_advance_timeout_millis: u64,
}

impl Default for ChassisBootConfig {
    fn default() -> Self {
        Self {
            workers: None,
            boot_manifest: None,
            lifecycle_advance_timeout_millis: DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS,
        }
    }
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

/// Assemble the chassis-wide [`KnownKeys`] set (ADR-0090 §4): every
/// migrated `*Layer::META` (http / gemini / anthropic / audio / fs /
/// render / actor-ring / scheduler-tuning / chassis-boot / window /
/// tick) plus
/// the hand-registered chassis knobs ([`CHASSIS_KNOBS`] — the RPC port
/// and the orphaned frame-size knob) and the runtime log-filter /
/// panic-hook knobs (`aether_substrate::runtime::RUNTIME_KNOBS`). e1's
/// [`validate_env`](aether_substrate::config::validate_env) sweeps the
/// process env against this; e2's `--print-config` dump walks the same
/// metas + records.
///
/// Lives bundle-side rather than in `aether-substrate::config` because
/// the cap layer types live in the per-cap crates, which depend on
/// `aether-substrate` (not the reverse) — the generic `known_keys`
/// assembly fn is in `config`; the concrete chassis registry is here.
#[must_use]
pub fn chassis_known_keys() -> KnownKeys {
    let (metas, records) = chassis_registry();
    known_keys(metas, &records)
}

/// The chassis-wide config registry: the migrated cap layer `Meta`s
/// (including the scheduler hot-path knobs' `SchedulerTuningConfigLayer`)
/// plus the hand-registered knob records (`CHASSIS_KNOBS` +
/// `aether_substrate::runtime::RUNTIME_KNOBS`). Shared by
/// [`chassis_known_keys`] (e1's sweep) and [`chassis_config_dump`] (e2's
/// `--print-config`) so both read one source of truth.
fn chassis_registry() -> (&'static [&'static Meta], Vec<KnobRecord>) {
    const METAS: &[&Meta] = &[
        &HttpConfigLayer::META,
        &HttpServerConfigLayer::META,
        &GeminiConfigLayer::META,
        &ContentGenConfigLayer::META,
        &AnthropicConfigLayer::META,
        &AudioConfigLayer::META,
        &NamespaceRootsLayer::META,
        &RenderTuningConfigLayer::META,
        &ActorRingConfigLayer::META,
        &SchedulerTuningConfigLayer::META,
        &SettlementConfigLayer::META,
        &ChassisBootConfigLayer::META,
        &WindowConfigLayer::META,
        &TickConfigLayer::META,
    ];
    let mut records: Vec<KnobRecord> = CHASSIS_KNOBS.to_vec();
    records.extend_from_slice(RUNTIME_KNOBS);
    (METAS, records)
}

/// Render the `--print-config` discovery dump for the full-stack chassis
/// (ADR-0090 §4): every cap layer knob + hand-registered knob with its
/// live source-resolved value, default, and doc. The chassis bins call
/// this when `--print-config` is passed and exit before boot.
#[must_use]
pub fn chassis_config_dump() -> String {
    let (metas, records) = chassis_registry();
    dump_config(metas, &records)
}

/// The hub chassis config registry: the full shared [`chassis_registry`]
/// plus the two hub-only additions — `EngineConfigLayer::META` (the
/// engines-cap heartbeat / proxy / disk-budget knobs only the hub wires)
/// and the `AETHER_ENGINE_STORE_ROOT` hand knob (the engines cap's
/// inline ops override, issue 1968). The hub reads the *full fleet* set
/// rather than only its own knobs because a hub-spawned substrate
/// inherits the hub's process environment, so fleet-wide cap knobs
/// legitimately sit in the hub's env destined for the spawned engine.
/// Shared by [`hub_known_keys`] (e1's sweep) and [`hub_config_dump`]
/// (e2's `--print-config`) so both read one source of truth (ADR-0090 §4).
fn hub_chassis_registry() -> (Vec<&'static Meta>, Vec<KnobRecord>) {
    let (metas, mut records) = chassis_registry();
    let mut metas: Vec<&'static Meta> = metas.to_vec();
    metas.push(&EngineConfigLayer::META);
    records.push(KnobRecord {
        env_key: "AETHER_ENGINE_STORE_ROOT",
        doc: "Parent directory for the engines cap's per-engine handle-store dirs; ops escape \
              hatch (unset falls back to the platform data dir, then the system temp dir).",
        default: None,
        kind: KnobKind::HandRegistered,
    });
    (metas, records)
}

/// Assemble the hub chassis [`KnownKeys`] set (ADR-0090 §4): the full
/// fleet registry (`hub_chassis_registry`). The hub's boot sweep
/// (`HubEnv::from_env_with_argv`) validates the process env against
/// this, mirroring the desktop / headless sweeps.
#[must_use]
pub fn hub_known_keys() -> KnownKeys {
    let (metas, records) = hub_chassis_registry();
    known_keys(&metas, &records)
}

/// Render the `--print-config` discovery dump for the hub chassis
/// (ADR-0090 §4): the full fleet registry plus the hub-only engine
/// knobs, so an operator sees every knob that will take effect on the
/// hub and the substrates it spawns.
#[must_use]
pub fn hub_config_dump() -> String {
    let (metas, records) = hub_chassis_registry();
    dump_config(&metas, &records)
}

/// Build the single-stage lifecycle config the headless chassis runs
/// (ADR-0082 PR 3b): a `Tick` self-loop with a `Quit` escape to a
/// `Shutdown` terminal. Components subscribe the `Tick` stage directly
/// on `aether.lifecycle` (ADR-0082 §7/§11), so the config wires no
/// initial subscribers. Desktop and `substrate_bench` run the three-stage
/// `Tick → Render → Present` graph from `frame_lifecycle_config()`
/// below instead.
///
/// `advance_timeout_millis` is the resolved value from
/// [`ChassisBootConfig::lifecycle_advance_timeout_millis`] (or
/// [`LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT`] for the substrate-bench).
///
/// # Panics
/// Panics if the (compile-time-fixed) graph fails to build — it can't,
/// the shape is structurally valid; the `expect` documents the
/// invariant.
#[must_use]
pub fn tick_only_lifecycle_config(advance_timeout_millis: u64) -> LifecycleConfig {
    let graph = LifecycleGraphData::builder()
        .state::<Tick>()
        .next::<Tick>()
        .quit::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Tick>()
        .build()
        .expect("tick-only lifecycle graph is structurally valid");
    LifecycleConfig { graph, initial_subscribers: vec![], advance_timeout_millis }
}

/// Build the three-stage frame lifecycle config the display-driving
/// chassis share (ADR-0082 §11, issues 1378 + 1489):
/// `Tick → Render → Present → Tick` (looping), with the `Quit` escape to
/// a `Shutdown` terminal on the `Present` stage. The chassis drives a
/// full `Tick → Render → Present` cycle per frame; `Render` broadcasts
/// only after the entire `Tick` chain has settled (ADR-0080 §6), so a
/// render producer's `on_render` runs once every actor's per-frame
/// `Tick` compute is done — no submitting against half-updated
/// cross-actor state.
///
/// The `Quit` escape lives on `Present`, not `Tick`: a `quit_pending`
/// flag set mid-frame is consumed only once the cap reaches `Present`,
/// so the in-flight frame has broadcast its full `Tick → Render →
/// Present` cycle before the lifecycle advances to `Shutdown` (ADR-0082
/// §3 "drain the frame before exit"). `Present` is a chassis-GPU-work
/// ordering point with an empty subscriber set today — it exists to host
/// this drain edge; per-stage component subscription lands when a
/// producer needs a post-`Render` hook.
///
/// Like [`tick_only_lifecycle_config`], components subscribe the `Tick`
/// (and `Render`) stage directly on `aether.lifecycle` (ADR-0082
/// §7/§11), so the config wires no initial subscribers. Desktop and
/// `substrate_bench` adopt this graph; headless stays
/// [`tick_only_lifecycle_config`] (its render cap is a no-op, so a
/// `Render` / `Present` stage would settle to no GPU work).
///
/// `advance_timeout_millis` is the resolved value from
/// [`ChassisBootConfig::lifecycle_advance_timeout_millis`] (or
/// [`LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT`] for the substrate-bench).
///
/// # Panics
/// Panics if the (compile-time-fixed) graph fails to build — it can't,
/// the shape is structurally valid; the `expect` documents the
/// invariant.
#[must_use]
pub fn frame_lifecycle_config(advance_timeout_millis: u64) -> LifecycleConfig {
    let graph = LifecycleGraphData::builder()
        .state::<Tick>()
        .next::<Render>()
        .state::<Render>()
        .next::<Present>()
        .state::<Present>()
        .next::<Tick>()
        .quit::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Tick>()
        .build()
        .expect("frame lifecycle graph is structurally valid");
    LifecycleConfig { graph, initial_subscribers: vec![], advance_timeout_millis }
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
    pub input_config: InputConfig,
    pub component_host_config: ComponentHostConfig,
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConfig,
    pub anthropic: AnthropicConfig,
    pub gemini: GeminiConfig,
    /// Content-gen staging config (ADR-0090). `with_common_caps` folds
    /// its `gen_dir` override (else the resolved `save`-namespace root)
    /// into the staging root threaded into the gemini cap.
    pub contentgen: ContentGenConfig,
    /// Trusted player-session wiring. The default has no configured `TurnSim`,
    /// so merely linking the common chassis opens no player listener.
    pub game_gateway: GameGatewayConfig,
}

/// Wire the aborter, worker count, and the common caps every full-
/// stack chassis carries. The renderer / window caps each chassis
/// adds after this in `.with_actor::<_>()` chains.
///
/// Boot order is declaration order. ADR-0081 retired the central
/// `LogCapability` — every actor owns its own per-actor log ring; no
/// boot ordering is needed for logging anymore.
pub fn with_common_caps<C: Chassis>(builder: Builder<C>, boot: CommonBoot) -> Builder<C> {
    // Resolve the content-gen staging root once, here, where the resolved
    // `NamespaceRoots.save` is in scope: the `AETHER_GEN_DIR` override wins,
    // else staging tracks the `save`-namespace root the fs cap already owns
    // (preserving its `AETHER_SAVE_DIR` → platform fallback without re-reading
    // env). Threaded into the gemini cap via `GeminiBoot`.
    let staging_root = boot.contentgen.gen_dir.clone().unwrap_or_else(|| boot.namespace_roots.save.clone());
    builder
        .with_aborter(boot.aborter)
        .with_workers(boot.workers)
        .with_ring_caps(boot.ring_caps)
        .with_scheduler_tuning(boot.scheduler_tuning)
        .with_teardown_cap(boot.teardown_cap)
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<InputCapability>(boot.input_config)
        .with_actor::<ComponentHostCapability>(boot.component_host_config)
        .with_actor::<FsCapability>(boot.namespace_roots)
        .with_actor::<TextCapability>(())
        .with_actor::<InventoryCapability>(())
        .with_actor::<HttpCapability>(boot.http)
        .with_actor::<TcpCapability>(())
        .with_actor::<GameGatewayCapability>(boot.game_gateway)
        .with_actor::<AnthropicCapability>(boot.anthropic)
        .with_actor::<GeminiCapability>(GeminiBoot { config: boot.gemini, gen_root: staging_root })
}

/// The mailbox namespaces `with_common_caps` registers — the linked
/// capabilities every full-stack chassis carries, for the `--describe`
/// manifest (ADR-0115, issue 1953). Read straight off each cap type's
/// `Addressable::NAMESPACE` const, so the values can't drift from what
/// `with_common_caps` actually claims; the *membership* of this list must
/// be kept in lockstep with the `.with_actor::<_>()` chain above (a cap
/// added there must be added here). The renderer / window / lifecycle
/// extras each chassis layers on top are appended by its own
/// `cap_namespaces` helper.
#[must_use]
pub fn common_cap_namespaces() -> Vec<&'static str> {
    vec![
        <TraceDispatchCapability as Addressable>::NAMESPACE,
        <InputCapability as Addressable>::NAMESPACE,
        <ComponentHostCapability as Addressable>::NAMESPACE,
        <FsCapability as Addressable>::NAMESPACE,
        <TextCapability as Addressable>::NAMESPACE,
        <InventoryCapability as Addressable>::NAMESPACE,
        <HttpCapability as Addressable>::NAMESPACE,
        <TcpCapability as Addressable>::NAMESPACE,
        <GameGatewayCapability as Addressable>::NAMESPACE,
        <AnthropicCapability as Addressable>::NAMESPACE,
        <GeminiCapability as Addressable>::NAMESPACE,
    ]
}

/// Assemble a chassis bin's `--describe` [`BinaryManifest`] (ADR-0115,
/// issue 1953): the chassis profile, the mailbox namespaces it links, and
/// the build provenance `build.rs` baked into the bundle crate
/// (`AETHER_GIT_SHA` / `AETHER_BUILD_PROFILE` / `AETHER_TARGET_TRIPLE`).
/// The `env!`s resolve in this crate, where `build.rs` set them. Each
/// chassis bin calls this on `--describe`, prints the JSON, and exits
/// before boot — the hub's binary store forks `<binary> --describe` once
/// at upload time to capture exactly this.
#[must_use]
pub fn binary_manifest(chassis: &str, caps: Vec<&'static str>) -> BinaryManifest {
    BinaryManifest {
        chassis: chassis.to_owned(),
        caps: caps.into_iter().map(str::to_owned).collect(),
        git_sha: env!("AETHER_GIT_SHA").to_owned(),
        profile: env!("AETHER_BUILD_PROFILE").to_owned(),
        target: env!("AETHER_TARGET_TRIPLE").to_owned(),
    }
}

/// Issue 763 P2: boot the RPC server only when `rpc_addr` is set,
/// mirroring the hub chassis. Substrate becomes an RPC server peer
/// that a hub (or any client) connects out to. `engine_name`
/// identifies the chassis profile in the `HelloAck` peer-kind.
pub fn maybe_with_rpc_server<C: Chassis>(
    builder: Builder<C>,
    rpc_addr: Option<SocketAddr>,
    engine_name: &str,
) -> Builder<C> {
    let Some(rpc_addr) = rpc_addr else {
        return builder;
    };
    builder.with_actor::<RpcServerCapability>(RpcServerConfig {
        bind_addr: rpc_addr.to_string(),
        peer_kind: PeerKind::Substrate {
            engine_name: engine_name.into(),
            engine_version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![],
        },
        // A forked substrate peer never fields engine-addressed forwards
        // (only the hub does), so it needs no route target.
        route_target: None,
    })
}

/// Issue 1761: boot the HTTP server only when `config` is `Some` (i.e.
/// the cap's `enabled` flag is set). Mirrors [`maybe_with_rpc_server`]:
/// an unconfigured chassis binds nothing.
pub fn maybe_with_http_server<C: Chassis>(builder: Builder<C>, config: Option<HttpServerConfig>) -> Builder<C> {
    let Some(config) = config else {
        return builder;
    };
    builder.with_actor::<HttpServerCapability>(config)
}

#[cfg(test)]
mod tests {
    use super::ActorRingConfig;
    use super::ActorRingConfigLayer;
    use super::ChassisBootConfig;
    use super::ChassisBootConfigLayer;
    use super::DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS;
    use super::DEFAULT_SETTLEMENT_CAP_SECS;
    use super::SchedulerTuningConfigLayer;
    use super::SettlementConfig;
    use super::chassis_known_keys;
    use super::file_section;
    use aether_actor::log::DEFAULT_RING_CAP;
    use aether_actor::trace::{DEFAULT_TRACE_RING_CAP, DEFAULT_TRACE_RING_MAX_CAP};
    use aether_lifecycle::LifecycleConfig;
    use aether_substrate::SchedulerTuning;
    use aether_substrate::config::ConfigError;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::Mutex;
    use std::sync::PoisonError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

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
    fn actor_ring_keys_are_known() {
        // The two ring env keys join the chassis known-key set so the
        // unknown-AETHER_* sweep (e1) doesn't warn on them.
        let known = chassis_known_keys();
        assert!(known.contains("AETHER_ACTOR_LOG_RING_SIZE"));
        assert!(known.contains("AETHER_ACTOR_TRACE_RING_SIZE"));
        assert!(known.contains("AETHER_ACTOR_TRACE_RING_MAX_SIZE"));
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
    fn scheduler_tuning_keys_are_known() {
        // The nine scheduler hot-path env keys join the chassis known-key
        // set via `SchedulerTuningConfigLayer::META` (they rode
        // `SCHEDULER_KNOBS` before issue 2485), so the unknown-AETHER_*
        // sweep (e1) doesn't flag them and the `--print-config` dump lists them.
        let known = chassis_known_keys();
        for key in [
            "AETHER_SPIN_WINDOW_USEC",
            "AETHER_LOCAL_STICKY_MAX",
            "AETHER_LOCAL_TIME_BUDGET_US",
            "AETHER_PEER_STEAL",
            "AETHER_LOCAL_CHAIN_BACKSTOP",
            "AETHER_HANDOFF_COST_NS",
            "AETHER_BLOB_RECRUIT_MIN",
            "AETHER_BLOB_RECRUIT_MAX",
            "AETHER_WAKE_COST_NANOS",
        ] {
            assert!(known.contains(key), "{key} must be a known scheduler key");
        }
    }

    #[test]
    fn settlement_to_cap_maps_seconds_and_zero_sentinel() {
        // Issue 2062 — the only logic this knob owns: seconds → `Duration`,
        // with `0` as the "no cap — wait forever" sentinel. Constructed
        // directly, so the test exercises our `to_cap`, not confique's
        // env/argv resolution (which the derive macro generates and
        // confique's own tests cover).
        assert_eq!(SettlementConfig { cap_secs: 0 }.to_cap(), Duration::MAX, "0 → wait forever");
        assert_eq!(SettlementConfig { cap_secs: 45 }.to_cap(), Duration::from_secs(45),);
        assert_eq!(SettlementConfig::default().to_cap(), Duration::from_secs(DEFAULT_SETTLEMENT_CAP_SECS),);
    }

    #[test]
    fn settlement_key_is_known() {
        // Guards the one-line registration of `SettlementConfigLayer::META`
        // in the chassis registry: without it the cap env key trips the
        // unknown-AETHER_* boot warn (e1). The production desktop/headless
        // gates don't read the knob yet (issue 2062 §Side findings: a
        // follow-up adopts it), so this registration is the only thing
        // keeping the key claimed.
        let known = chassis_known_keys();
        assert!(known.contains("AETHER_SETTLEMENT_CAP_SECS"));
    }

    #[test]
    fn chassis_boot_config_defaults_match() {
        use confique::Config as _;
        // No `.env()` source: literal defaults only — env-free. The
        // `default = 1000` literal must equal `LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT`
        // so an unset knob reproduces the cap's const default.
        // Tripwire: drifts when the producing const or the derive literal changes.
        let _guard = RING_ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let layer = ChassisBootConfigLayer::builder().load().expect("defaults load");
        assert_eq!(
            layer.lifecycle_advance_timeout_millis, DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS,
            "derive default must match DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS",
        );
        assert_eq!(
            DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS,
            LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT,
            "DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS must match LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT",
        );
        let default = ChassisBootConfig::default();
        assert_eq!(default.lifecycle_advance_timeout_millis, DEFAULT_LIFECYCLE_ADVANCE_TIMEOUT_MS,);
        assert_eq!(default.workers, None);
        assert_eq!(default.boot_manifest, None);
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
    fn frame_lifecycle_graph_is_tick_render_present_with_shutdown_terminal() {
        // ADR-0082 §11 / issues 1378 + 1489: the display-driving chassis
        // graph is `Tick → Render → Present → Tick` (looping) with the
        // `Quit` escape to a `Shutdown` terminal on the `Present` stage.
        // The graph's edge accessors (`next` / `quit` per state) are
        // crate-private to `aether-lifecycle`, so this crate-boundary
        // check reads the public `Debug` (start + the non-terminal state
        // kinds + terminals) plus the now-empty `initial_subscribers`
        // set. Quit-edge *placement* (on `Present`, not `Tick`) is
        // verified at the cap-unit layer (`lifecycle.rs` `resolve_edge`
        // tests, which can read `state().quit`) and end-to-end by the
        // `substrate_bench` quit-drain scenario.
        use aether_data::Kind;
        use aether_kinds::{Present, Render, Shutdown, Tick};

        let cfg = super::frame_lifecycle_config(LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT);
        let graph_dbg = format!("{:?}", cfg.graph);
        let tick = format!("{:?}", <Tick as Kind>::ID);
        let render = format!("{:?}", <Render as Kind>::ID);
        let present = format!("{:?}", <Present as Kind>::ID);
        let shutdown = format!("{:?}", <Shutdown as Kind>::ID);

        // Start state is Tick.
        assert!(graph_dbg.contains(&format!("start: {tick}")), "expected start Tick in {graph_dbg}");
        // Tick, Render, and Present are all non-terminal states.
        assert!(graph_dbg.contains(&render), "expected a Render state in {graph_dbg}");
        assert!(graph_dbg.contains(&present), "expected a Present state in {graph_dbg}");
        // Shutdown is the sole terminal.
        assert!(graph_dbg.contains(&format!("terminals: [{shutdown}]")), "expected Shutdown terminal in {graph_dbg}");

        // No initial subscribers: components subscribe the `Tick` stage
        // directly on `aether.lifecycle` (ADR-0082 §7/§11); the boot-time
        // `Tick → aether.input` relay retired with the input cap's
        // `on_tick` fan-out.
        assert!(cfg.initial_subscribers.is_empty());
    }

    #[test]
    fn chassis_known_keys_includes_scheduler_hot_path_knobs() {
        // ADR-0090 unit b2: the scheduler hot-path knobs join the
        // known-key set, so e1's unknown-AETHER_ sweep doesn't flag them.
        let known = chassis_known_keys();
        for key in
            ["AETHER_LOCAL_STICKY_MAX", "AETHER_LOCAL_TIME_BUDGET_US", "AETHER_PEER_STEAL", "AETHER_HANDOFF_COST_NS"]
        {
            assert!(known.contains(key), "chassis_known_keys missing {key}");
        }
    }

    #[test]
    fn common_manifest_includes_the_inert_game_gateway() {
        use aether_actor::Addressable;
        use aether_game::GameGatewayCapability;

        assert!(
            super::common_cap_namespaces().contains(&GameGatewayCapability::NAMESPACE),
            "every common chassis must register and advertise the game gateway identity",
        );
    }

    #[test]
    fn chassis_known_keys_includes_infra_knobs() {
        // Tripwire: catches a future registry refactor that drops the
        // RUNTIME_KNOBS extend or the frame-size record, re-introducing
        // the false `validate_env` "unknown key" warning for these five
        // legitimately-documented process-level knobs.
        let known = chassis_known_keys();
        for key in [
            "AETHER_LOG_FILTER",
            "AETHER_BACKTRACE",
            "AETHER_CRASH_LOG_DISABLE",
            "AETHER_CRASH_LOG_DIR",
            "AETHER_MAX_FRAME_SIZE",
        ] {
            assert!(known.contains(key), "chassis_known_keys missing {key}");
        }
    }

    #[test]
    fn hub_known_keys_includes_engine_and_fleet_and_store_root() {
        // The hub composition this crate owns: the shared registry
        // unioned with the two hub-only additions — the engines-cap
        // `EngineConfigLayer::META` keys, the `AETHER_ENGINE_STORE_ROOT`
        // hand knob, and (because a hub-spawned substrate inherits the
        // hub's env) the fleet cap keys the shared registry carries.
        let known = super::hub_known_keys();
        assert!(
            known.contains("AETHER_HUB_HEARTBEAT_INTERVAL_SECS"),
            "AETHER_HUB_HEARTBEAT_INTERVAL_SECS must be a known hub key",
        );
        assert!(known.contains("AETHER_ENGINE_STORE_ROOT"), "AETHER_ENGINE_STORE_ROOT must be a known hub key");
        assert!(
            known.contains("AETHER_AUDIO_DISABLE"),
            "fleet cap keys must stay in the hub known-key set (spawned substrates inherit \
             the hub's env)",
        );
    }

    #[test]
    fn hub_config_dump_lists_an_engine_knob() {
        // e2's hub `--print-config` walks the same registry as e1's sweep, so
        // the hub-only engine knobs render in the dump.
        let dump = super::hub_config_dump();
        assert!(dump.contains("AETHER_HUB_HEARTBEAT_INTERVAL_SECS"));
    }

    #[test]
    fn chassis_boot_config_keys_are_known() {
        // Guards the three `ChassisBootConfigLayer::META` keys joining
        // the chassis known-key set. `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`
        // relocated here from the scheduler knob list (it was only
        // registered scheduler-side because the old capabilities monolith
        // couldn't hold it; the bundle can, so the workaround is gone).
        let known = chassis_known_keys();
        assert!(known.contains("AETHER_WORKERS"), "AETHER_WORKERS must be a known key");
        assert!(known.contains("AETHER_BOOT_MANIFEST"), "AETHER_BOOT_MANIFEST must be a known key");
        assert!(
            known.contains("AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS"),
            "AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS must be a known key",
        );
    }

    #[test]
    fn chassis_known_keys_includes_a_representative_cap_key() {
        // The cap layer META walk lands the per-cap env keys (a
        // representative from each migrated cap) plus the derive-Config
        // chassis knobs — the set is non-empty and covers more than scheduler.
        let known = chassis_known_keys();
        assert!(known.contains("AETHER_HTTP_DISABLE"));
        assert!(known.contains("AETHER_GEN_DIR"));
        assert!(known.contains("AETHER_WORKERS"));
        assert!(!known.is_empty());
    }

    #[test]
    fn chassis_known_keys_includes_http_server_keys() {
        // Issue 1761: `HttpServerConfigLayer::META` must join the
        // chassis registry so the unknown-AETHER_* sweep (e1) doesn't
        // flag `AETHER_HTTP_SERVER_*` env vars set by operators.
        let known = chassis_known_keys();
        assert!(known.contains("AETHER_HTTP_SERVER_ENABLED"), "AETHER_HTTP_SERVER_ENABLED must be a known key");
        assert!(known.contains("AETHER_HTTP_SERVER_BIND_ADDR"), "AETHER_HTTP_SERVER_BIND_ADDR must be a known key");
    }

    #[test]
    fn chassis_config_dump_lists_a_knob_from_each_cap_plus_scheduler() {
        // ADR-0090 §4 `--print-config`: the dump walks the same registry as
        // the sweep, so it lists a representative knob from each cap, a
        // chassis-boot knob, and a scheduler hot-path knob — with a
        // header row.
        let dump = super::chassis_config_dump();
        assert!(dump.contains("KEY"));
        assert!(dump.contains("AETHER_HTTP_DISABLE")); // http cap
        assert!(dump.contains("AETHER_HTTP_SERVER_BIND_ADDR")); // http server cap
        assert!(dump.contains("AETHER_GEMINI_TIMEOUT_MS")); // gemini cap
        // Tripwire: the content-gen staging knob is only in the dump if
        // `ContentGenConfigLayer::META` stays registered in `chassis_registry`.
        assert!(dump.contains("AETHER_GEN_DIR")); // content-gen staging cap
        assert!(dump.contains("AETHER_AUDIO_DISABLE")); // audio cap
        assert!(dump.contains("AETHER_WORKERS")); // chassis-boot derive-Config knob
        assert!(dump.contains("AETHER_LOCAL_STICKY_MAX")); // scheduler knob
        assert!(dump.contains("AETHER_LOG_FILTER")); // runtime knob
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

    /// Regression guard for the enable / disable convention (#1791): a
    /// capability's enable/disable flag is resolved through its
    /// derive-`Config` (`*Config::from_argv_then_env`), never a raw
    /// `env::var` read in a chassis builder. This is the shape #1761 put
    /// the http server on; the guard keeps a future cap from regressing to
    /// presence-inference or a hand-rolled env read. The chassis window /
    /// tick / boot knobs are now also derive-`Config` (`WindowConfig`,
    /// `TickConfig`, `ChassisBootConfig`), so no raw `env::var` of any
    /// known `AETHER_*` key should appear in the chassis builder sources.
    #[test]
    fn chassis_builders_resolve_cap_enable_flags_via_config() {
        // Enable / disable env keys owned by a derive-`Config` cap. Add a
        // cap's flag key here when a new opt-in / opt-out cap lands.
        const CAP_FLAG_KEYS: &[&str] = &["AETHER_HTTP_SERVER_ENABLED", "AETHER_AUDIO_DISABLE"];
        let desktop = include_str!("desktop/chassis.rs");
        let headless = include_str!("headless/chassis.rs");
        for key in CAP_FLAG_KEYS {
            let raw_read = format!("env::var(\"{key}\")");
            for (chassis, src) in [("desktop", desktop), ("headless", headless)] {
                assert!(
                    !src.contains(&raw_read),
                    "{chassis} chassis reads {key} via raw env::var — route it through the \
                     cap's config API instead (see the `config` module's \
                     \"Enable / disable convention\")",
                );
            }
        }
    }
}
