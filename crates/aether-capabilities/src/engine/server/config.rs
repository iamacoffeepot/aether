//! Engines-cap configuration (ADR-0090) — the liveness-heartbeat
//! tuning plus the hub binary store's layout dir, disk budget, and
//! bootstrap list. Native-only: resolved by the hub chassis and handed
//! into [`EngineServer::init`](super::EngineServer) via
//! `with_actor::<EngineServer>(cfg)`.

use crate::engine::proxy::HeartbeatParams;
use crate::engine::store::DEFAULT_DISK_BUDGET_BYTES;
use std::collections::HashSet;
use std::time::Duration;

/// Default total time a freshly-forked substrate's proxy keeps
/// retrying its startup dial before giving up (issue 2072). A debug
/// cold start fork+exec+bind can stretch well past a healthy
/// localhost dial when many substrates come up at once and
/// oversubscribe the cores (e.g. a concurrent `FleetBench` fleet), so
/// the budget is generous — far longer than a single cold start
/// needs, comfortably under the `FleetBench` client's own spawn cap so
/// the hub returns a clean `Err` first rather than the client
/// tripping its backstop. `0` is the wait-forever sentinel.
pub(super) const DEFAULT_PROXY_CONNECT_BUDGET_SECS: u64 = 30;

/// Resolved engines-cap configuration (ADR-0090, issue 1339): the
/// liveness-heartbeat tuning plus the hub binary store's layout dir,
/// disk budget, and bootstrap list (ADR-0115, #1954 — these last three
/// moved onto the config off their pre-ADR-0090 naked `env::var`
/// readers). The inline `AETHER_ENGINE_STORE_ROOT` reader
/// (`engine_store_root`) is a separate, still-inline knob.
///
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `EngineConfigLayer`, the clap-shaped `EngineOverlay`, and the
/// inherent `from_env` / `from_argv_then_env` shims (argv beats env
/// beats the literal default). The hub chassis resolves it with
/// `EngineConfig::from_argv_then_env(cli.engine.into_layer())` and
/// hands it to `with_actor::<EngineServer>(cfg)`; tests build it
/// directly. `env_prefix = "AETHER_HUB"` + the `heartbeat_*` /
/// `binary_disk_budget_bytes` field names compose the
/// `AETHER_HUB_HEARTBEAT_*` / `AETHER_HUB_BINARY_DISK_BUDGET_BYTES`
/// env keys and `--hub-*` flags; `binary_store_dir` / `binary_bootstrap`
/// pin the unprefixed `AETHER_BINARY_STORE_DIR` / `AETHER_BINARY_BOOTSTRAP`
/// keys via per-field `env` overrides. `Default` (the test constructor)
/// resolves the heartbeat to `0/0` (disabled) and the store fields to
/// unset / `16 GiB`; production picks up the `default = 5/3` / `16 GiB`
/// literals and the env layers through `from_argv_then_env`.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_HUB", cli_prefix = "hub")]
pub struct EngineConfig {
    /// Heartbeat ping cadence in seconds
    /// (`AETHER_HUB_HEARTBEAT_INTERVAL_SECS` /
    /// `--hub-heartbeat-interval-secs`). `0` disables the heartbeat
    /// entirely (engines are then only evicted on a
    /// connection-close, never on a wedge).
    #[config(default = 5)]
    pub heartbeat_interval_secs: u64,
    /// Consecutive missed pings that mark an engine dead
    /// (`AETHER_HUB_HEARTBEAT_MISS_LIMIT` /
    /// `--hub-heartbeat-miss-limit`). Small N tolerates a transient
    /// hiccup; `0` also disables the heartbeat. Detection latency is
    /// `miss_limit × interval_secs`.
    #[config(default = 3)]
    pub heartbeat_miss_limit: u32,
    /// Total seconds a freshly-forked substrate's proxy keeps
    /// retrying its startup dial before the spawn fails
    /// (`AETHER_HUB_PROXY_CONNECT_BUDGET_SECS` /
    /// `--hub-proxy-connect-budget-secs`, issue 2072). Generous by
    /// default so a debug cold start under fork contention isn't
    /// called dead prematurely; `0` is the wait-forever sentinel
    /// (retry until the dial succeeds or hits a terminal error).
    #[config(default = 30)]
    pub proxy_connect_budget_secs: u64,
    /// How many times `on_spawn` re-forks a substrate on a fresh port
    /// before giving up (`AETHER_HUB_PROXY_SPAWN_ATTEMPTS` /
    /// `--hub-proxy-spawn-attempts`, issue 2422). A freshly-forked
    /// substrate can lose its guessed RPC port to another socket in
    /// `free_local_port`'s TOCTOU window and exit on a fatal bind; a
    /// re-fork on a fresh port escapes the stolen port, since the theft
    /// is per-port and independent across attempts, so N attempts drop
    /// the failure probability geometrically. `1` preserves the
    /// single-attempt behavior (no re-fork).
    #[config(default = 3)]
    pub proxy_spawn_attempts: u32,
    /// Layout-root override for the hub's content-addressed binary
    /// store (`AETHER_BINARY_STORE_DIR`, unprefixed — the ops escape
    /// hatch and the fleet tests' per-process isolation knob). Unset
    /// (`None`) → the computed default `data_dir/aether/binaries/v1`
    /// (`ArtifactStore::default_root`). A bare `Option<String>` (not a
    /// `PathBuf`) keeps that runtime-computed default in `init`, so
    /// `EngineConfig` needs no `skip_from_layer`; `EngineServer::init`
    /// joins the store's layout-version dir to a set override.
    #[config(env = "AETHER_BINARY_STORE_DIR")]
    pub binary_store_dir: Option<String>,
    /// On-disk byte budget for the binary store
    /// (`AETHER_HUB_BINARY_DISK_BUDGET_BYTES`, derived from the
    /// `AETHER_HUB` prefix / `--hub-binary-disk-budget-bytes`). Default
    /// 16 GiB (`DEFAULT_DISK_BUDGET_BYTES`); LRU eviction over unpinned,
    /// unnamed entries holds it.
    #[config(default = 17_179_869_184u64)]
    pub binary_disk_budget_bytes: u64,
    /// Chassis bins to bootstrap-ingest at init so a `default` / `name`
    /// selector resolves in a fresh or `restart-hub`'d hub
    /// (`AETHER_BINARY_BOOTSTRAP`, unprefixed, comma-separated). Each is
    /// ingested content-addressed and named by its file stem;
    /// idempotent via content dedup. `ensure-tunnel.sh` exports the
    /// freshly-built chassis bins here.
    #[config(env = "AETHER_BINARY_BOOTSTRAP", default = [], csv_set)]
    pub binary_bootstrap: HashSet<String>,
}

impl Default for EngineConfig {
    /// The test constructor: heartbeat disabled (`0/0`) but a real
    /// `DEFAULT_DISK_BUDGET_BYTES` budget — `0` is inert for the
    /// heartbeat (no pinging) yet destructive for the store (it would
    /// evict every unnamed upload), so the budget can't share the
    /// heartbeat's zero. Store dir unset (the computed default) and an
    /// empty bootstrap. Production resolves all five through the layer
    /// (`from_argv_then_env`); this matches the prior `from_env()`
    /// store budget every `EngineConfig::default()` consumer saw.
    fn default() -> Self {
        Self {
            heartbeat_interval_secs: 0,
            heartbeat_miss_limit: 0,
            // A real budget (not the heartbeat's inert `0`): tests fork
            // real substrates, so the proxy needs a generous-but-finite
            // startup-dial budget — `0` would mean wait-forever and
            // hang on a genuinely dead substrate.
            proxy_connect_budget_secs: DEFAULT_PROXY_CONNECT_BUDGET_SECS,
            // A single attempt by default in tests: the re-fork loop is
            // a contention mitigation, and tests fork real substrates
            // serially, so one attempt keeps the path deterministic.
            proxy_spawn_attempts: 1,
            binary_store_dir: None,
            binary_disk_budget_bytes: DEFAULT_DISK_BUDGET_BYTES,
            binary_bootstrap: HashSet::new(),
        }
    }
}

impl EngineConfig {
    /// The [`HeartbeatParams`] to arm each proxy with, or `None`
    /// when the heartbeat is disabled (`0` interval or miss limit).
    pub(super) fn heartbeat_params(&self) -> Option<HeartbeatParams> {
        if self.heartbeat_interval_secs == 0 || self.heartbeat_miss_limit == 0 {
            None
        } else {
            Some(HeartbeatParams {
                interval: Duration::from_secs(self.heartbeat_interval_secs),
                miss_limit: self.heartbeat_miss_limit,
            })
        }
    }

    /// The startup-dial connect budget to arm each spawned proxy
    /// with (issue 2072). `Some(d)` caps the retry; `None` (the `0`
    /// sentinel) means wait forever.
    pub(super) fn connect_budget(&self) -> Option<Duration> {
        (self.proxy_connect_budget_secs != 0)
            .then(|| Duration::from_secs(self.proxy_connect_budget_secs))
    }

    /// The bounded re-fork attempt count for `on_spawn` (issue 2422),
    /// clamped to at least 1 — `0` would never fork at all.
    pub(super) fn spawn_attempts(&self) -> u32 {
        self.proxy_spawn_attempts.max(1)
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// The connect budget resolves a non-zero seconds value to a
    /// finite `Duration`, and `0` to the wait-forever sentinel `None`.
    #[test]
    fn connect_budget_maps_zero_to_wait_forever() {
        let finite = EngineConfig {
            proxy_connect_budget_secs: 12,
            ..EngineConfig::default()
        };
        assert_eq!(finite.connect_budget(), Some(Duration::from_secs(12)));
        let forever = EngineConfig {
            proxy_connect_budget_secs: 0,
            ..EngineConfig::default()
        };
        assert_eq!(forever.connect_budget(), None);
    }

    /// The default budget is generous and finite — never the
    /// wait-forever sentinel — so a debug cold start under fork
    /// contention isn't called dead prematurely, while a genuinely
    /// dead substrate still fails the spawn rather than hanging.
    #[test]
    fn default_connect_budget_is_generous_and_finite() {
        let budget = EngineConfig::default()
            .connect_budget()
            .expect("default budget is finite, not wait-forever");
        assert_eq!(
            budget,
            Duration::from_secs(DEFAULT_PROXY_CONNECT_BUDGET_SECS)
        );
        assert!(budget >= Duration::from_secs(30), "default stays generous");
    }
}
