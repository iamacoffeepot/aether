//! Engines-cap configuration (ADR-0090) — the liveness-heartbeat
//! tuning plus the hub binary store's layout dir, disk budget, and
//! bootstrap list, and the per-engine spawn-dir parent. Native-only:
//! resolved by the hub chassis and handed into
//! [`FleetServer::init`](super::FleetServer) via
//! `with_actor::<FleetServer>(cfg)`.

use crate::proxy::HeartbeatParams;
use crate::store::DEFAULT_DISK_BUDGET_BYTES;
use std::collections::HashSet;
use std::time::Duration;

/// Default total time a freshly-forked substrate's proxy keeps
/// retrying its startup dial before giving up (issue 2072). A debug
/// cold start fork+exec+bind can stretch well past a healthy
/// localhost dial when many substrates come up at once and
/// oversubscribe the cores (e.g. a concurrent `FleetHarness` fleet), so
/// the budget is generous — far longer than a single cold start
/// needs, comfortably under the `FleetHarness` client's own spawn cap so
/// the hub returns a clean `Err` first rather than the client
/// tripping its backstop. `0` is the wait-forever sentinel.
const DEFAULT_PROXY_CONNECT_BUDGET_SECS: u64 = 30;

/// Default settle time between observing an engine death and re-forking
/// it. Long enough that a crash-on-boot substrate cannot spin the cap,
/// short enough that a real recovery is not noticeably delayed.
const DEFAULT_RESTART_BACKOFF_MILLIS: u64 = 500;

/// Default restart budget per engine, and the window it is counted over:
/// five starts in five minutes, the start-limit shape this repository's
/// own systemd unit uses for the same crash-loop question.
const DEFAULT_RESTART_BURST_LIMIT: u32 = 5;
const DEFAULT_RESTART_BURST_WINDOW_SECS: u64 = 300;

/// Resolved automatic-restart supervision policy, or `None` when the cap
/// is not supervising restarts at all.
///
/// Assembled once at init from [`FleetConfig::restart_policy`]. The cap
/// holds the `Option` rather than the raw flags, so every restart site
/// reads one value and a disabled policy has no reachable restart code
/// beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Settle time between observing a death and re-forking, so a
    /// substrate that dies immediately on boot cannot spin the cap.
    pub backoff: Duration,
    /// Most automatic restarts one engine lineage may spend inside
    /// [`Self::burst_window`] before the cap gives up on it.
    pub burst_limit: u32,
    /// Rolling window the burst limit is counted over. Restarts older
    /// than this stop counting, so an engine that crashes once a day
    /// is restarted every time while one crash-looping is not.
    pub burst_window: Duration,
}

/// Resolved engines-cap configuration (ADR-0090, issue 1339): the
/// liveness-heartbeat tuning plus the hub binary store's layout dir,
/// disk budget, and bootstrap list (ADR-0115, #1954 — these last three
/// moved onto the config off their pre-ADR-0090 naked `env::var`
/// readers), plus the per-engine spawn-dir parent
/// (`fleet_store_root`, #2482 — the last of the sweep).
///
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `FleetConfigLayer`, the clap-shaped `FleetOverlay`, and the
/// inherent `from_env` / `from_argv_then_env` shims (argv beats env
/// beats the literal default). The hub chassis resolves it with
/// `FleetConfig::from_argv_then_env(cli.fleet.into_layer())` and
/// hands it to `with_actor::<FleetServer>(cfg)`; tests build it
/// directly. `env_prefix = "AETHER_HUB"` + the `heartbeat_*` /
/// `binary_disk_budget_bytes` field names compose the
/// `AETHER_HUB_HEARTBEAT_*` / `AETHER_HUB_BINARY_DISK_BUDGET_BYTES`
/// env keys and `--hub-*` flags; `binary_store_dir` / `fleet_store_root`
/// / `binary_bootstrap` pin the unprefixed `AETHER_BINARY_STORE_DIR` /
/// `AETHER_FLEET_STORE_ROOT` / `AETHER_BINARY_BOOTSTRAP` keys via
/// per-field `env` overrides. `Default` (the test constructor)
/// resolves the heartbeat to `0/0` (disabled) and the store fields to
/// unset / `16 GiB`; production picks up the `default = 5/3` / `16 GiB`
/// literals and the env layers through `from_argv_then_env`.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_HUB", cli_prefix = "hub")]
pub struct FleetConfig {
    /// Heartbeat ping interval in seconds; 0 disables liveness checks.
    ///
    /// `0` disables the heartbeat entirely (engines are then only evicted
    /// on a connection-close, never on a wedge).
    #[config(default = 5)]
    pub heartbeat_interval_secs: u64,
    /// Consecutive missed pings before an engine is declared dead.
    ///
    /// Small N tolerates a transient hiccup; `0` also disables the
    /// heartbeat. Detection latency is `miss_limit × interval_secs`.
    #[config(default = 3)]
    pub heartbeat_miss_limit: u32,
    /// Seconds a freshly-spawned engine may take to connect before the spawn fails.
    ///
    /// The proxy keeps retrying its startup dial for this budget (issue
    /// 2072). Generous by default so a debug cold start under fork
    /// contention isn't called dead prematurely; `0` is the wait-forever
    /// sentinel (retry until the dial succeeds or hits a terminal error).
    #[config(default = 30)]
    pub proxy_connect_budget_secs: u64,
    /// How many times a failed engine spawn is retried on a fresh port.
    ///
    /// `on_spawn` re-forks a substrate on a fresh port up to this many
    /// times before giving up (issue 2422). A freshly-forked substrate
    /// can lose its guessed RPC port to another socket in
    /// `free_local_port`'s TOCTOU window and exit on a fatal bind; a
    /// re-fork on a fresh port escapes the stolen port, since the theft is
    /// per-port and independent across attempts, so N attempts drop the
    /// failure probability geometrically. `1` preserves the
    /// single-attempt behavior (no re-fork).
    #[config(default = 3)]
    pub proxy_spawn_attempts: u32,
    /// Whether a crashed or evicted engine is automatically re-spawned.
    ///
    /// Off by default: the cap's historical contract is that a death is
    /// terminal and the caller re-spawns, and every existing harness and
    /// test reads it that way. Turning it on makes the cap re-fork a
    /// `Crashed` or `Evicted` engine from the recipe it was spawned with.
    /// A deliberate `TerminateEngine` is never restarted whatever this
    /// says — the operator asked for the engine to be gone.
    #[config(default = false)]
    pub restart_on_crash: bool,
    /// Milliseconds to wait after a death before re-forking the engine.
    ///
    /// A settle window, not a retry schedule: a substrate that dies on
    /// boot would otherwise be re-forked as fast as the cap can observe
    /// it. The burst limit, not this, is what stops a persistent crash
    /// loop. Ignored when `restart_on_crash` is off.
    #[config(default = 500)]
    pub restart_backoff_millis: u64,
    /// Most automatic restarts of one engine within the burst window.
    ///
    /// Past this the cap stops restarting that engine, logs loudly, and
    /// records the death normally. `5` in `restart_burst_window_secs`
    /// mirrors the start-limit shape the repository's own systemd unit
    /// uses. `0` disables restarts as surely as `restart_on_crash =
    /// false`.
    #[config(default = 5)]
    pub restart_burst_limit: u32,
    /// Rolling window, in seconds, the restart burst limit is counted over.
    ///
    /// Restarts older than this stop counting toward the limit, so an
    /// engine that crashes rarely is always recovered while one
    /// crash-looping exhausts its budget and stays dead.
    #[config(default = 300)]
    pub restart_burst_window_secs: u64,
    /// Directory for the hub's content-addressed binary store; unset uses the platform data dir.
    ///
    /// The ops escape hatch and the fleet tests' per-process isolation
    /// knob. Unset → the computed default `data_dir/aether/binaries/v1`
    /// (`ArtifactStore::default_root`). A bare `Option<String>` (not a
    /// `PathBuf`) keeps that runtime-computed default in `init`, so
    /// `FleetConfig` needs no `skip_from_layer`; `FleetServer::init`
    /// joins the store's layout-version dir to a set override.
    #[config(env = "AETHER_BINARY_STORE_DIR")]
    pub binary_store_dir: Option<String>,
    /// Parent directory for per-engine spawn and handle-store dirs; unset uses the platform data dir.
    ///
    /// The ops escape hatch and the fleet tests' per-process isolation
    /// knob (issue 1274). Unset → `dirs::data_dir().join("aether/engines")`,
    /// falling back to `std::env::temp_dir().join("aether-fleets")` if no
    /// data dir is resolvable. A bare `Option<String>` (not a `PathBuf`)
    /// keeps that runtime-computed fallback chain in `init`, so
    /// `FleetConfig` needs no `skip_from_layer`; `FleetServer::init`
    /// resolves it once into `FleetServerState::fleet_store_root`.
    #[config(env = "AETHER_FLEET_STORE_ROOT")]
    pub fleet_store_root: Option<String>,
    /// On-disk byte budget for the binary store.
    ///
    /// Default 16 GiB (`DEFAULT_DISK_BUDGET_BYTES`); LRU eviction over
    /// unpinned, unnamed entries holds it.
    #[config(default = 17_179_869_184u64)]
    pub binary_disk_budget_bytes: u64,
    /// Chassis binaries to ingest at startup so a name selector resolves in a fresh hub.
    ///
    /// A comma-separated list bootstrap-ingested at init so a `default` /
    /// `name` selector resolves in a fresh or `restart-hub`'d hub. Each is
    /// ingested content-addressed and named by its file stem; idempotent
    /// via content dedup. `ensure-tunnel.sh` exports the freshly-built
    /// chassis bins here.
    #[config(env = "AETHER_BINARY_BOOTSTRAP", default = [], csv_set)]
    pub binary_bootstrap: HashSet<String>,
}

impl Default for FleetConfig {
    /// The test constructor: heartbeat disabled (`0/0`) but a real
    /// `DEFAULT_DISK_BUDGET_BYTES` budget — `0` is inert for the
    /// heartbeat (no pinging) yet destructive for the store (it would
    /// evict every unnamed upload), so the budget can't share the
    /// heartbeat's zero. Store dir unset (the computed default) and an
    /// empty bootstrap. Production resolves all five through the layer
    /// (`from_argv_then_env`); this matches the prior `from_env()`
    /// store budget every `FleetConfig::default()` consumer saw.
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
            // Restart supervision stays off in the test constructor for
            // the same reason it is off in production by default: every
            // existing harness reads a death as terminal, and a cap that
            // silently re-forked would change what those tests observe.
            restart_on_crash: false,
            restart_backoff_millis: DEFAULT_RESTART_BACKOFF_MILLIS,
            restart_burst_limit: DEFAULT_RESTART_BURST_LIMIT,
            restart_burst_window_secs: DEFAULT_RESTART_BURST_WINDOW_SECS,
            binary_store_dir: None,
            fleet_store_root: None,
            binary_disk_budget_bytes: DEFAULT_DISK_BUDGET_BYTES,
            binary_bootstrap: HashSet::new(),
        }
    }
}

impl FleetConfig {
    /// The `HeartbeatParams` to arm each proxy with, or `None`
    /// when the heartbeat is disabled (`0` interval or miss limit).
    #[must_use]
    pub fn heartbeat_params(&self) -> Option<HeartbeatParams> {
        (self.heartbeat_interval_secs != 0 && self.heartbeat_miss_limit != 0).then(|| HeartbeatParams {
            interval: Duration::from_secs(self.heartbeat_interval_secs),
            miss_limit: self.heartbeat_miss_limit,
        })
    }

    /// The startup-dial connect budget to arm each spawned proxy
    /// with (issue 2072). `Some(d)` caps the retry; `None` (the `0`
    /// sentinel) means wait forever.
    #[must_use]
    pub fn connect_budget(&self) -> Option<Duration> {
        (self.proxy_connect_budget_secs != 0).then(|| Duration::from_secs(self.proxy_connect_budget_secs))
    }

    /// The bounded re-fork attempt count for `on_spawn` (issue 2422),
    /// clamped to at least 1 — `0` would never fork at all.
    #[must_use]
    pub fn spawn_attempts(&self) -> u32 {
        self.proxy_spawn_attempts.max(1)
    }

    /// The automatic-restart policy to supervise deaths under, or `None`
    /// when the cap should leave a dead engine dead.
    ///
    /// Both the opt-in flag and a non-zero burst limit are required: a
    /// limit of `0` admits no restart at all, so resolving it to a live
    /// policy would arm the machinery around a budget that can never be
    /// spent. Collapsing both to `None` keeps one disabled state instead
    /// of two that behave alike but read differently.
    #[must_use]
    pub fn restart_policy(&self) -> Option<RestartPolicy> {
        (self.restart_on_crash && self.restart_burst_limit != 0).then(|| RestartPolicy {
            backoff: Duration::from_millis(self.restart_backoff_millis),
            burst_limit: self.restart_burst_limit,
            burst_window: Duration::from_secs(self.restart_burst_window_secs),
        })
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// The connect budget resolves a non-zero seconds value to a
    /// finite `Duration`, and `0` to the wait-forever sentinel `None`.
    #[test]
    fn connect_budget_maps_zero_to_wait_forever() {
        let finite = FleetConfig { proxy_connect_budget_secs: 12, ..FleetConfig::default() };
        assert_eq!(finite.connect_budget(), Some(Duration::from_secs(12)));
        let forever = FleetConfig { proxy_connect_budget_secs: 0, ..FleetConfig::default() };
        assert_eq!(forever.connect_budget(), None);
    }

    /// The default budget is generous and finite — never the
    /// wait-forever sentinel — so a debug cold start under fork
    /// contention isn't called dead prematurely, while a genuinely
    /// dead substrate still fails the spawn rather than hanging.
    /// Restart supervision is opt-in, and stays off through *either*
    /// disabling route. The default config must not arm it — the whole
    /// crate's existing contract is that a death is terminal — and a
    /// `restart_on_crash` turned on beside a zero burst limit must
    /// resolve to the same `None` rather than arming machinery around a
    /// budget that admits nothing.
    #[test]
    fn restart_policy_is_off_by_default_and_off_at_a_zero_burst_limit() {
        assert_eq!(FleetConfig::default().restart_policy(), None, "restart supervision must be opt-in");

        let zero_budget = FleetConfig { restart_on_crash: true, restart_burst_limit: 0, ..FleetConfig::default() };
        assert_eq!(zero_budget.restart_policy(), None, "a zero burst limit disables restarts as surely as the flag");
    }

    /// An enabled policy carries the configured milliseconds and seconds
    /// through to the `Duration`s the cap actually waits and measures on.
    /// Catches a unit slip at the one place the raw numbers become time.
    #[test]
    fn an_enabled_restart_policy_carries_its_configured_durations() {
        let config = FleetConfig {
            restart_on_crash: true,
            restart_backoff_millis: 750,
            restart_burst_limit: 3,
            restart_burst_window_secs: 60,
            ..FleetConfig::default()
        };
        let policy = config.restart_policy().expect("an enabled flag with a non-zero budget yields a policy");

        assert_eq!(policy.backoff, Duration::from_millis(750), "backoff is milliseconds");
        assert_eq!(policy.burst_limit, 3);
        assert_eq!(policy.burst_window, Duration::from_mins(1), "the burst window is seconds");
    }

    #[test]
    fn default_connect_budget_is_generous_and_finite() {
        let budget = FleetConfig::default().connect_budget().expect("default budget is finite, not wait-forever");
        assert_eq!(budget, Duration::from_secs(DEFAULT_PROXY_CONNECT_BUDGET_SECS));
        assert!(budget >= Duration::from_secs(30), "default stays generous");
    }
}
