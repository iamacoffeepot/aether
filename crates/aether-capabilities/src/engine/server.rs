//! `aether.engine` — engines capability (issue 763 P4).
//!
//! A `#[bridge(singleton)]` `NativeActor` that supervises a fleet of
//! `EngineProxy` actors — the engine-management surface of the
//! forward-model architecture (issue 763). Three handlers:
//!
//! - **`on_spawn`** ([`SpawnEngine`]) picks a free localhost port,
//!   fork+execs the substrate binary with `AETHER_RPC_PORT` injected,
//!   then boots an `aether.engine.proxy:<id>` child actor that dials
//!   it. The proxy owns the forked child from there — startup-dial
//!   retry, kill-on-failed-boot, kill-on-drop. Reply:
//!   `SpawnEngineResult`.
//! - **`on_list`** ([`ListEngines`]) reports every supervised engine.
//! - **`on_terminate`** ([`TerminateEngine`]) forwards the kind to the
//!   engine's proxy (which SIGKILLs its substrate and self-shuts-down)
//!   and drops the table entry. Reply: `TerminateEngineResult`.
//!
//! ## Scope (issue 763 P4 vs P5)
//!
//! P4 is the cap itself: spawn / list / terminate. The hub RPC
//! server's `engine = Some(_)` routing — which drives `ForwardEnvelope`
//! at a proxy on behalf of an external RPC client — and the
//! `describe_kinds` / `describe_component` proxy handlers land in P5
//! alongside the `aether-mcp` extraction; they only have meaning once
//! an out-of-process RPC client drives the hub.
//!
//! Native-only: the cap fork+execs processes and threads the
//! `std::process::Child` handle into the proxy. The `#[bridge]` macro
//! emits the wasm-side marker stub so `aether-capabilities` still
//! compiles for `wasm32`.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root — the
// `#[bridge]` macro emits `impl HandlesKind<K>` markers as siblings of
// the mod.
use aether_kinds::{
    EngineAlive, EngineDied, ListComponentBinaries, ListEngineBinaries, ListEngines,
    ResolveComponent, RouteEnvelope, SpawnEngine, TerminateEngine, UploadBinary, UploadComponent,
};
#[cfg(test)]
use std::sync::{Arc, Mutex};

// `EngineConfig` (+ its derive-emitted `EngineOverlay`) ride through
// file root for the hub chassis bin, which flattens the overlay into
// `HubCli`, resolves argv-then-env, and passes the config to
// `with_actor::<EngineServer>(cfg)` (ADR-0090). Native-only re-export —
// the engines cap is native-only, so the config has no wasm consumer.
#[cfg(not(target_arch = "wasm32"))]
pub use server_native::{EngineConfig, EngineOverlay};

#[aether_actor::bridge(singleton)]
mod server_native {
    use super::{
        EngineAlive, EngineDied, ListComponentBinaries, ListEngineBinaries, ListEngines,
        ResolveComponent, RouteEnvelope, SpawnEngine, TerminateEngine, UploadBinary,
        UploadComponent,
    };
    use crate::engine::proxy::{EngineProxy, EngineProxyConfig, HeartbeatParams};
    use crate::store::{
        ArtifactKind, ArtifactStore, DEFAULT_DISK_BUDGET_BYTES, LAYOUT_VERSION_DIR, Selector,
        StoredArtifact, StoredManifest, component_manifest,
    };
    use aether_actor::actor;
    use aether_data::{EngineId, Kind, MailboxId, Uuid};
    use aether_kinds::{
        BinaryManifest, BinarySelector, CallSettled, ComponentSelector, DeadEngineDescriptor,
        DeathReason, EngineDescriptor, ForwardEnvelope, ListComponentBinariesResult,
        ListEngineBinariesResult, ListEnginesResult, ResolveComponentResult, SpawnEngineResult,
        TerminateEngineResult, UploadBinaryResult, UploadComponentResult,
    };
    use aether_substrate::Mail;
    use aether_substrate::Subname;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::{Source, SourceAddr};
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::env;
    use std::fs;
    use std::io;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Env override for the parent directory under which the cap
    /// allocates per-engine handle-store dirs (issue 1274). Absent →
    /// fall through to `dirs::data_dir().join("aether/engines")`, then
    /// to `std::env::temp_dir().join("aether-engines")` if no data dir
    /// is resolvable.
    const ENV_ENGINE_STORE_ROOT: &str = "AETHER_ENGINE_STORE_ROOT";

    /// The chassis a `default` selector (an empty [`BinarySelector::query`]
    /// with no attribute filters) resolves to (ADR-0115): `headless` has no
    /// window and runs on any host, so a bare spawn is self-sufficient.
    const DEFAULT_CHASSIS: &str = "headless";

    /// Default heartbeat ping cadence (issue 1339). 5 s × the miss
    /// limit is the detection-latency vs. flap-tolerance tradeoff.
    const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 5;
    /// Default consecutive-miss threshold before an engine is declared
    /// dead. Small N tolerates a transient hiccup / GC pause.
    const DEFAULT_HEARTBEAT_MISS_LIMIT: u32 = 3;

    /// Default total time a freshly-forked substrate's proxy keeps
    /// retrying its startup dial before giving up (issue 2072). A debug
    /// cold start fork+exec+bind can stretch well past a healthy
    /// localhost dial when many substrates come up at once and
    /// oversubscribe the cores (e.g. a concurrent `FleetBench` fleet), so
    /// the budget is generous — far longer than a single cold start
    /// needs, comfortably under the `FleetBench` client's own spawn cap so
    /// the hub returns a clean `Err` first rather than the client
    /// tripping its backstop. `0` is the wait-forever sentinel.
    const DEFAULT_PROXY_CONNECT_BUDGET_SECS: u64 = 30;

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
        #[config(default = 5, parse = parse_heartbeat_interval_secs)]
        pub heartbeat_interval_secs: u64,
        /// Consecutive missed pings that mark an engine dead
        /// (`AETHER_HUB_HEARTBEAT_MISS_LIMIT` /
        /// `--hub-heartbeat-miss-limit`). Small N tolerates a transient
        /// hiccup; `0` also disables the heartbeat. Detection latency is
        /// `miss_limit × interval_secs`.
        #[config(default = 3, parse = parse_heartbeat_miss_limit)]
        pub heartbeat_miss_limit: u32,
        /// Total seconds a freshly-forked substrate's proxy keeps
        /// retrying its startup dial before the spawn fails
        /// (`AETHER_HUB_PROXY_CONNECT_BUDGET_SECS` /
        /// `--hub-proxy-connect-budget-secs`, issue 2072). Generous by
        /// default so a debug cold start under fork contention isn't
        /// called dead prematurely; `0` is the wait-forever sentinel
        /// (retry until the dial succeeds or hits a terminal error).
        #[config(default = 30, parse = parse_proxy_connect_budget_secs)]
        pub proxy_connect_budget_secs: u64,
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
        #[config(default = 17_179_869_184u64, parse = parse_binary_disk_budget)]
        pub binary_disk_budget_bytes: u64,
        /// Chassis bins to bootstrap-ingest at init so a `default` / `name`
        /// selector resolves in a fresh or `restart-hub`'d hub
        /// (`AETHER_BINARY_BOOTSTRAP`, unprefixed, comma-separated). Each is
        /// ingested content-addressed and named by its file stem;
        /// idempotent via content dedup. `ensure-tunnel.sh` exports the
        /// freshly-built chassis bins here.
        #[config(
            env = "AETHER_BINARY_BOOTSTRAP",
            default = [],
            parse = parse_binary_bootstrap,
            csv_set
        )]
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
                binary_store_dir: None,
                binary_disk_budget_bytes: DEFAULT_DISK_BUDGET_BYTES,
                binary_bootstrap: HashSet::new(),
            }
        }
    }

    impl EngineConfig {
        /// The [`HeartbeatParams`] to arm each proxy with, or `None`
        /// when the heartbeat is disabled (`0` interval or miss limit).
        fn heartbeat_params(&self) -> Option<HeartbeatParams> {
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
        fn connect_budget(&self) -> Option<Duration> {
            (self.proxy_connect_budget_secs != 0)
                .then(|| Duration::from_secs(self.proxy_connect_budget_secs))
        }
    }

    // confique's `parse_env` contract is `fn(&str) -> Result<T, impl
    // Error>`; these total helpers carry a `Result` they never fill with
    // `Err` — an unparseable value folds back to the default (soft, like
    // the DAG validator's caps; the ADR-0090 §4 strict/erroring variant
    // is a follow-up). Hence the `unnecessary_wraps` allow.

    /// Parse the heartbeat interval; unparseable → the default.
    #[allow(clippy::unnecessary_wraps)]
    fn parse_heartbeat_interval_secs(s: &str) -> Result<u64, Infallible> {
        Ok(s.trim().parse().unwrap_or(DEFAULT_HEARTBEAT_INTERVAL_SECS))
    }

    /// Parse the heartbeat miss limit; unparseable → the default.
    #[allow(clippy::unnecessary_wraps)]
    fn parse_heartbeat_miss_limit(s: &str) -> Result<u32, Infallible> {
        Ok(s.trim().parse().unwrap_or(DEFAULT_HEARTBEAT_MISS_LIMIT))
    }

    /// Parse the proxy connect budget seconds; unparseable → the default.
    #[allow(clippy::unnecessary_wraps)]
    fn parse_proxy_connect_budget_secs(s: &str) -> Result<u64, Infallible> {
        Ok(s.trim()
            .parse()
            .unwrap_or(DEFAULT_PROXY_CONNECT_BUDGET_SECS))
    }

    /// Parse the binary-store disk budget; unparseable → the default
    /// `DEFAULT_DISK_BUDGET_BYTES` (mirrors the heartbeat parsers).
    #[allow(clippy::unnecessary_wraps)]
    fn parse_binary_disk_budget(s: &str) -> Result<u64, Infallible> {
        Ok(s.trim().parse().unwrap_or(DEFAULT_DISK_BUDGET_BYTES))
    }

    /// Split a comma-separated bootstrap path list, trimming and dropping
    /// empties (mirrors the http allowlist's `parse_allowlist`). Total —
    /// never errors.
    #[allow(clippy::unnecessary_wraps)]
    fn parse_binary_bootstrap(s: &str) -> Result<HashSet<String>, Infallible> {
        Ok(s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect())
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

    /// How many recently-died engines [`EngineServer`] retains for
    /// `list_engines`' `recently_died` sidecar (issue 1906). A small
    /// bound: the surface is "what just left and why", not an audit log —
    /// the oldest record is dropped once the ring is full.
    const RECENTLY_DIED_CAP: usize = 16;

    /// One recently-departed engine in [`EngineServer`]'s recently-died
    /// ring (issue 1906). Cap-internal — holds the wire fields plus the
    /// `Instant` the cap removed the engine, so `on_list` can compute the
    /// `died_age_millis` it reports in a [`DeadEngineDescriptor`].
    struct DeadRecord {
        engine_id: String,
        rpc_port: u16,
        reason: DeathReason,
        died_at: Instant,
    }

    /// One supervised engine in [`EngineServer`]'s table.
    struct EngineEntry {
        /// Mailbox of the `aether.engine.proxy:<id>` actor — the
        /// forward target for [`TerminateEngine`].
        proxy_mailbox: MailboxId,
        /// The localhost RPC port the cap assigned this substrate.
        rpc_port: u16,
        /// When the cap last saw this engine alive (issue 1339): set at
        /// spawn (just-connected = alive) and refreshed on each
        /// `EngineAlive` the proxy reports from a confirmed `Pong`.
        /// `on_list` reports `now - last_alive` as the heartbeat age.
        last_alive: Instant,
    }

    /// Engines capability: supervises a fleet of [`EngineProxy`]
    /// actors, one per spawned substrate. Singleton at `aether.engine`.
    pub struct EngineServer {
        engines: HashMap<EngineId, EngineEntry>,
        /// Monotonic source of `EngineId`s. Engine ids only need to be
        /// unique among the engines this cap currently supervises — a
        /// process-local counter delivers that without a `uuid` rng
        /// dependency. Starts at 1 (`Uuid::from_u128(0)` is the nil
        /// uuid).
        next_engine_seq: u128,
        /// Cached so `on_route` can push a `ForwardEnvelope` at a proxy
        /// while *propagating the inbound reply-to* — `NativeCtx`'s
        /// sends stamp the cap as sender, but a routed call's reply
        /// must reach the originating `RpcServerCapability`, not here.
        mailer: Arc<Mailer>,
        /// Liveness-heartbeat tuning each spawned proxy is armed with
        /// (issue 1339), resolved once from [`EngineConfig`] at init.
        /// `None` disables the heartbeat fleet-wide.
        heartbeat: Option<HeartbeatParams>,
        /// Startup-dial connect budget each spawned proxy is armed with
        /// (issue 2072), resolved once from [`EngineConfig`] at init.
        /// `Some(d)` caps the retry; `None` is the wait-forever sentinel.
        connect_budget: Option<Duration>,
        /// Bounded ring of the last [`RECENTLY_DIED_CAP`] engines that
        /// left the table and why (issue 1906). `on_terminate` /
        /// `on_engine_died` push a [`DeadRecord`] at the removal site;
        /// `on_list` renders it into the reply's `recently_died` sidecar
        /// so an observer can tell a clean terminate from a crash or a
        /// heartbeat eviction.
        recently_died: VecDeque<DeadRecord>,
        /// Hub-scoped content-addressed binary store (ADR-0115, issue
        /// 1953) — the storage half of the artifact registry.
        /// `on_upload_binary` ingests a staged binary content-addressed;
        /// `on_list_engine_binaries` enumerates the stored entries. Built from
        /// `EngineConfig` (the layout dir + disk budget) at init so it
        /// persists across a `restart-hub` (the layout root outlives the
        /// hub child); the spawn cutover (#1954) reads it back through the
        /// store's `get` seam.
        store: ArtifactStore,
    }

    #[actor]
    impl NativeActor for EngineServer {
        type Config = EngineConfig;
        const NAMESPACE: &'static str = "aether.engine";

        fn init(config: EngineConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            // Build the hub-scoped store from `EngineConfig` (ADR-0090): the
            // layout-dir override + disk budget ride config fields (their
            // `AETHER_BINARY_*` env keys are the config env layer), then
            // bootstrap-ingest the chassis bins in `binary_bootstrap` so
            // `default` / `name` resolve in a fresh or `restart-hub`'d hub
            // (ADR-0115, #1954). An unset store dir falls back to the
            // computed default; a set one gets the layout-version dir joined
            // (matching the prior `AETHER_BINARY_STORE_DIR` reader).
            let store_dir = config
                .binary_store_dir
                .as_deref()
                .filter(|d| !d.is_empty())
                .map_or_else(ArtifactStore::default_root, |dir| {
                    PathBuf::from(dir).join(LAYOUT_VERSION_DIR)
                });
            let mut store = ArtifactStore::open(&store_dir, config.binary_disk_budget_bytes);
            bootstrap_ingest(&mut store, &config.binary_bootstrap);
            Ok(Self {
                engines: HashMap::new(),
                next_engine_seq: 1,
                mailer: ctx.mailer(),
                heartbeat: config.heartbeat_params(),
                connect_budget: config.connect_budget(),
                recently_died: VecDeque::new(),
                store,
            })
        }

        /// Push a [`DeadRecord`] onto the recently-died ring, evicting the
        /// oldest entry once the ring is full (issue 1906).
        fn record_death(&mut self, engine_id: String, rpc_port: u16, reason: DeathReason) {
            if self.recently_died.len() >= RECENTLY_DIED_CAP {
                self.recently_died.pop_front();
            }
            self.recently_died.push_back(DeadRecord {
                engine_id,
                rpc_port,
                reason,
                died_at: Instant::now(),
            });
        }

        /// Enumerate every engine the cap currently supervises.
        ///
        /// # Agent
        /// Send `ListEngines` (fieldless). Reply: `ListEnginesResult
        /// { engines: [{ engine_id, rpc_port, last_heartbeat_age_millis }],
        /// recently_died: [{ engine_id, rpc_port, reason, died_age_millis }] }`.
        #[handler]
        fn on_list(&mut self, _ctx: &mut NativeCtx<'_>, _mail: ListEngines) -> ListEnginesResult {
            let now = Instant::now();
            let engines = self
                .engines
                .iter()
                .map(|(id, entry)| EngineDescriptor {
                    engine_id: id.0.to_string(),
                    rpc_port: entry.rpc_port,
                    last_heartbeat_age_millis: u64::try_from(
                        now.saturating_duration_since(entry.last_alive).as_millis(),
                    )
                    .unwrap_or(u64::MAX),
                })
                .collect();
            let recently_died = self
                .recently_died
                .iter()
                .map(|record| DeadEngineDescriptor {
                    engine_id: record.engine_id.clone(),
                    rpc_port: record.rpc_port,
                    reason: record.reason.clone(),
                    died_age_millis: u64::try_from(
                        now.saturating_duration_since(record.died_at).as_millis(),
                    )
                    .unwrap_or(u64::MAX),
                })
                .collect();
            ListEnginesResult {
                engines,
                recently_died,
            }
        }

        /// Fork+exec a substrate binary and connect a proxy to it.
        ///
        /// # Agent
        /// Send `SpawnEngine { selector, args, boot_manifest }`. The cap
        /// resolves `selector` against its content-addressed binary store
        /// (ADR-0115), materializes the resolved bytes to an executable
        /// temp file, assigns a free localhost port for the substrate's
        /// RPC server, injects it as `AETHER_RPC_PORT`, forks the realized
        /// binary, then boots an `aether.engine.proxy:<id>` actor that
        /// dials it. Reply: `SpawnEngineResult::Ok { engine_id, rpc_port }`
        /// on success, or `Err { error }` if the selector resolves to no
        /// stored binary, the fork fails, or the substrate never comes up.
        #[handler]
        fn on_spawn(&mut self, ctx: &mut NativeCtx<'_>, mail: SpawnEngine) -> SpawnEngineResult {
            // Resolve the registry selector to stored content bytes before
            // any side effect, so a miss returns without reserving a port
            // or burning an engine id (ADR-0115, #1954).
            let Some(artifact) = resolve_selector(&mut self.store, &mail.selector) else {
                return SpawnEngineResult::Err {
                    error: format!(
                        "no binary in the registry matched selector {:?}",
                        mail.selector
                    ),
                };
            };

            let rpc_port = match free_local_port() {
                Ok(port) => port,
                Err(e) => {
                    return SpawnEngineResult::Err {
                        error: format!("could not allocate an RPC port: {e}"),
                    };
                }
            };

            // Allocate a unique scratch subdirectory per spawned engine to
            // hold its materialized executable.
            let engine_id = EngineId(Uuid::from_u128(self.next_engine_seq));
            self.next_engine_seq += 1;
            let engine_store_dir = engine_store_root().join(engine_id.0.simple().to_string());

            // Stored bytes are content-addressed and not directly
            // fork-exec'able, so materialize the resolved entry to an
            // executable temp file under this engine's scratch dir and fork
            // that (ADR-0115 §Execution); the caller never sees the path.
            let exec_path = engine_store_dir.join("substrate");
            if let Err(e) = realize_executable(&artifact.path, &exec_path) {
                return SpawnEngineResult::Err {
                    error: format!(
                        "materializing binary {} to {}: {e}",
                        artifact.hash,
                        exec_path.display()
                    ),
                };
            }

            let mut command = Command::new(&exec_path);
            command
                .args(&mail.args)
                .env("AETHER_RPC_PORT", rpc_port.to_string())
                .stdin(Stdio::null());
            // A spawn carrying a component list rides in as a boot-manifest
            // path; inject it the same way as the RPC port so the spawned
            // chassis reads the listed wasm itself and comes up with those
            // components already loading (issue 1776).
            if let Some(boot_manifest) = &mail.boot_manifest {
                command.env("AETHER_BOOT_MANIFEST", boot_manifest);
            }
            let child = match command.spawn() {
                Ok(child) => child,
                Err(e) => {
                    return SpawnEngineResult::Err {
                        error: format!("failed to spawn {}: {e}", exec_path.display()),
                    };
                }
            };

            let subname = engine_id.0.simple().to_string();
            let rpc_addr = format!("127.0.0.1:{rpc_port}");

            // `finish()` runs `EngineProxy::init` on this thread: it
            // dials the substrate (retrying while it comes up) and, on
            // failure, kills the child it was handed — so a failed
            // spawn never leaves an orphan for the cap to clean up.
            let result = ctx
                .spawn_child::<EngineProxy>(
                    Subname::Named(&subname),
                    EngineProxyConfig {
                        engine_id,
                        rpc_addr,
                        spawned: Some(child),
                        heartbeat: self.heartbeat,
                        connect_budget: self.connect_budget,
                    },
                )
                .finish();

            match result {
                Ok(proxy_mailbox) => {
                    self.engines.insert(
                        engine_id,
                        EngineEntry {
                            proxy_mailbox,
                            rpc_port,
                            // Just connected = alive; the heartbeat
                            // refreshes this on each confirmed Pong.
                            last_alive: Instant::now(),
                        },
                    );
                    SpawnEngineResult::Ok {
                        engine_id: engine_id.0.to_string(),
                        rpc_port,
                    }
                }
                Err(e) => SpawnEngineResult::Err {
                    error: format!("proxy failed to connect to the spawned substrate: {e:?}"),
                },
            }
        }

        /// Terminate a supervised engine.
        ///
        /// # Agent
        /// Send `TerminateEngine { engine_id }` (the string from a
        /// prior `SpawnEngineResult` / `ListEnginesResult`). The cap
        /// forwards the kind to the engine's proxy — which SIGKILLs
        /// its substrate and self-shuts-down — and drops its table
        /// entry. Reply: `TerminateEngineResult::Ok`, or `Err { error }`
        /// for an `engine_id` that doesn't parse or names no
        /// supervised engine.
        #[handler]
        fn on_terminate(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            mail: TerminateEngine,
        ) -> TerminateEngineResult {
            let engine_id = match Uuid::parse_str(&mail.engine_id) {
                Ok(uuid) => EngineId(uuid),
                Err(e) => {
                    return TerminateEngineResult::Err {
                        error: format!("engine_id {:?} is not a valid UUID: {e}", mail.engine_id),
                    };
                }
            };

            let Some(entry) = self.engines.remove(&engine_id) else {
                return TerminateEngineResult::Err {
                    error: format!("no supervised engine {}", mail.engine_id),
                };
            };

            // Record the deliberate shutdown in the recently-died ring so
            // `list_engines` can show it left cleanly (issue 1906). The
            // proxy deliberately does not `report_died` for a terminate —
            // the cap initiated it — so there is no second signal to
            // reconcile and this is the one record for this death.
            let proxy_mailbox = entry.proxy_mailbox;
            self.record_death(
                mail.engine_id.clone(),
                entry.rpc_port,
                DeathReason::Terminated,
            );

            // Forward to the proxy: it SIGKILLs its substrate and
            // self-shuts-down. Fire-and-forget — the proxy doesn't
            // reply, and the table entry is already gone, so the
            // returned MailId has nothing to subscribe against.
            let payload = mail.encode_into_bytes();
            let _ =
                ctx.send_envelope_traced(proxy_mailbox, <TerminateEngine as Kind>::ID, &payload);
            TerminateEngineResult::Ok
        }

        /// Relay one mail to a specific engine's substrate.
        ///
        /// # Agent
        /// Not a user-facing tool — the hub's `RpcServerCapability`
        /// sends this when an RPC client addresses a `Call` at
        /// `engine = Some(_)`. The cap looks the engine up in its
        /// table and re-emits a `ForwardEnvelope` at the matching
        /// `aether.engine.proxy:<id>`, propagating the inbound
        /// reply-to verbatim so the substrate's reply (and the proxy's
        /// terminal `CallSettled`) stream straight back to that
        /// `RpcServerCapability`. An unknown / unparseable `engine_id`
        /// is answered with `CallSettled::Err` so the originating wire
        /// call closes instead of hanging.
        #[handler]
        fn on_route(&mut self, ctx: &mut NativeCtx<'_>, mail: RouteEnvelope) {
            let reply_to = ctx.reply_target();
            let SourceAddr::Component(reply_target) = reply_to.addr else {
                // A routed call always carries a Component reply-to
                // (the originating RpcServerCapability). Without one
                // there's nowhere to stream the reply or the
                // CallSettled — drop rather than guess.
                tracing::warn!(
                    target: "aether_substrate::engine_server",
                    engine_id = %mail.engine_id,
                    "engine route: no Component reply-to; dropping",
                );
                return;
            };
            let correlation = reply_to.correlation_id;

            let engine_id = match Uuid::parse_str(&mail.engine_id) {
                Ok(uuid) => EngineId(uuid),
                Err(e) => {
                    settle_err(
                        &self.mailer,
                        reply_target,
                        correlation,
                        format!("engine_id {:?} is not a valid UUID: {e}", mail.engine_id),
                    );
                    return;
                }
            };
            let Some(entry) = self.engines.get(&engine_id) else {
                settle_err(
                    &self.mailer,
                    reply_target,
                    correlation,
                    format!("no supervised engine {}", mail.engine_id),
                );
                return;
            };

            // Re-emit as a ForwardEnvelope at the proxy, carrying the
            // inbound reply-to verbatim so the substrate's reply — and
            // the proxy's CallSettled — route straight back to the
            // originating RpcServerCapability.
            let forward = ForwardEnvelope {
                mailbox: mail.mailbox,
                kind: mail.kind,
                payload: mail.payload,
            };
            self.mailer.push(
                Mail::new(
                    entry.proxy_mailbox,
                    <ForwardEnvelope as Kind>::ID,
                    forward.encode_into_bytes(),
                    1,
                )
                .with_reply_to(reply_to),
            );
        }

        /// Evict a dead engine from the table (issue 1339).
        ///
        /// # Agent
        /// Not a user-facing tool — a proxy sends `EngineDied` when it
        /// observes its substrate's connection close or its liveness
        /// heartbeat cross the miss limit. The cap drops the table entry
        /// so `list_engines` stops reporting a corpse. Idempotent: a
        /// `died` for an already-removed engine (e.g. one a concurrent
        /// `terminate_substrate` already dropped) is a logged no-op, so
        /// it can't race the terminate path.
        #[handler]
        fn on_engine_died(&mut self, _ctx: &mut NativeCtx<'_>, mail: EngineDied) {
            let Ok(uuid) = Uuid::parse_str(&mail.engine_id) else {
                tracing::warn!(
                    target: "aether_substrate::engine_server",
                    engine_id = %mail.engine_id,
                    "engine died: unparseable engine_id; ignoring",
                );
                return;
            };
            if let Some(entry) = self.engines.remove(&EngineId(uuid)) {
                tracing::info!(
                    target: "aether_substrate::engine_server",
                    engine_id = %mail.engine_id,
                    reason = ?mail.reason,
                    "engine evicted: proxy reported death",
                );
                // Record inside the `is_some` guard so a duplicate `died`
                // (e.g. one a concurrent terminate already dropped) adds no
                // second record — one record per death (issue 1339/1906).
                let rpc_port = entry.rpc_port;
                self.record_death(mail.engine_id, rpc_port, mail.reason);
            }
        }

        /// Refresh an engine's last-seen-alive time (issue 1339).
        ///
        /// # Agent
        /// Not a user-facing tool — a proxy sends `EngineAlive` each
        /// time it confirms a heartbeat `Pong`. The cap stamps the
        /// table entry so `list_engines` reports a fresh
        /// `last_heartbeat_age_millis`. An `alive` for an unknown engine
        /// (already evicted) is a silent no-op.
        #[handler]
        fn on_engine_alive(&mut self, _ctx: &mut NativeCtx<'_>, mail: EngineAlive) {
            let Ok(uuid) = Uuid::parse_str(&mail.engine_id) else {
                return;
            };
            if let Some(entry) = self.engines.get_mut(&EngineId(uuid)) {
                entry.last_alive = Instant::now();
            }
        }

        /// Ingest a binary into the hub's content-addressed store.
        ///
        /// # Agent
        /// Send `UploadBinary { staged_path, name }`. The hub reads the
        /// staged path itself (aether-mcp never reads the bytes — too
        /// large for the tool channel), sha256-hashes it, dedups against
        /// the store, forks `staged_path --describe` to capture its
        /// `BinaryManifest`, stores both, and points `name` (when set) at
        /// the hash. Reply: `UploadBinaryResult::Ok { hash, name }`, or
        /// `Err { error }` for an unreadable path or a `--describe` that
        /// failed or didn't yield a parseable manifest.
        #[handler]
        fn on_upload_binary(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            mail: UploadBinary,
        ) -> UploadBinaryResult {
            match ingest_binary(&mut self.store, &mail.staged_path, mail.name.clone()) {
                Ok(hash) => UploadBinaryResult::Ok {
                    hash,
                    name: mail.name,
                },
                Err(error) => UploadBinaryResult::Err { error },
            }
        }

        /// Enumerate the hub's stored engine binaries.
        ///
        /// # Agent
        /// Send `ListEngineBinaries { chassis?, caps, target? }` (each filter
        /// field AND-combined; an absent / empty field is no constraint).
        /// Reply: `ListEngineBinariesResult { binaries: [{ hash, name,
        /// manifest: { chassis, caps, git_sha, profile, target } }] }`.
        #[handler]
        fn on_list_engine_binaries(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            mail: ListEngineBinaries,
        ) -> ListEngineBinariesResult {
            ListEngineBinariesResult {
                binaries: self.store.list_binaries(&mail),
            }
        }

        /// Ingest a component wasm into the hub's content-addressed store
        /// (ADR-0116, issue 1956).
        ///
        /// # Agent
        /// Send `UploadComponent { staged_path, name }`. The hub reads the
        /// staged path itself (aether-mcp never reads the bytes — too large
        /// for the tool channel), sha256-hashes it, dedups against the
        /// store, reads the manifest straight from the wasm (no execution
        /// step), stores both, and points `name` (when set) at the hash.
        /// Reply: `UploadComponentResult::Ok { hash, name }`, or
        /// `Err { error }` for an unreadable path or an unparseable wasm.
        #[handler]
        fn on_upload_component(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            mail: UploadComponent,
        ) -> UploadComponentResult {
            match ingest_component(&mut self.store, &mail.staged_path, mail.name.clone()) {
                Ok(hash) => UploadComponentResult::Ok {
                    hash,
                    name: mail.name,
                },
                Err(error) => UploadComponentResult::Err { error },
            }
        }

        /// Resolve a component selector to its wasm bytes + manifest.
        ///
        /// # Agent
        /// Send `ResolveComponent { selector }`. aether-mcp calls this
        /// hub-local before forwarding a `LoadComponent` to the target
        /// substrate, so the load seam stays path-free. The selector is a
        /// `hash` / `name` (latest) / `module@actor` exact token, or a
        /// namespace / handled-kind attribute query (an attribute query
        /// matching more than one component is a clean ambiguity error).
        /// Reply: `ResolveComponentResult::Ok { hash, wasm, name, manifest,
        /// export }`, or `Err { error }` for no match / ambiguity.
        #[handler]
        fn on_resolve_component(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            mail: ResolveComponent,
        ) -> ResolveComponentResult {
            resolve_component(&mut self.store, &mail.selector)
        }

        /// Enumerate the hub's stored component binaries.
        ///
        /// # Agent
        /// Send `ListComponentBinaries { namespace?, handled_kind? }` (each
        /// filter AND-combined; an absent field is no constraint). Reply:
        /// `ListComponentBinariesResult { components: [{ hash, name, manifest }] }`.
        #[handler]
        fn on_list_component_binaries(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            mail: ListComponentBinaries,
        ) -> ListComponentBinariesResult {
            ListComponentBinariesResult {
                components: self.store.list_components(&mail),
            }
        }
    }

    /// Fork `binary_path --describe` and parse the JSON manifest it prints
    /// (ADR-0115, issue 1953). The one-time capture of what a binary *is* —
    /// its chassis kind, linked caps, and build provenance — without the
    /// hub linking the chassis crate. `stdin` is nulled so a describe can't
    /// block on input.
    fn describe_binary(binary_path: &str) -> Result<BinaryManifest, String> {
        let output = Command::new(binary_path)
            .arg("--describe")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("forking {binary_path:?} --describe: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "{binary_path:?} --describe exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("parsing {binary_path:?} --describe manifest JSON: {e}"))
    }

    /// Ingest the binary at `path` into `store` content-addressed,
    /// capturing its manifest via a one-time `<path> --describe` fork
    /// (ADR-0115, issue 1953). Shared by the `on_upload_binary` handler and
    /// the [`bootstrap_ingest`] boot path. Returns the stored content hash,
    /// or a human-readable error for an unreadable path or a `--describe`
    /// that failed / yielded no parseable manifest. Idempotent — identical
    /// bytes dedup to the same hash.
    fn ingest_binary(
        store: &mut ArtifactStore,
        path: &str,
        name: Option<String>,
    ) -> Result<String, String> {
        let bytes = fs::read(path).map_err(|e| format!("reading binary path {path:?}: {e}"))?;
        let manifest = describe_binary(path)?;
        Ok(store.upload(
            &bytes,
            ArtifactKind::Binary,
            StoredManifest::Binary(manifest),
            name,
        ))
    }

    /// Bootstrap-ingest each chassis bin in `paths` into `store`, naming
    /// each by its file stem so a `default` / `name` selector resolves in a
    /// fresh or `restart-hub`'d hub (ADR-0115, issue 1954). The list rides
    /// `EngineConfig`'s `binary_bootstrap` field (its `AETHER_BINARY_BOOTSTRAP`
    /// env layer, ADR-0090). A path that can't be read or `--describe`d is
    /// logged and skipped — a bad bootstrap entry must not fail hub boot.
    /// Idempotent via content dedup.
    pub(super) fn bootstrap_ingest(store: &mut ArtifactStore, paths: &HashSet<String>) {
        for path_str in paths {
            let name = Path::new(path_str)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned);
            match ingest_binary(store, path_str, name) {
                Ok(hash) => tracing::info!(
                    target: "aether_substrate::engine_server",
                    path = path_str.as_str(),
                    hash = %hash,
                    "binary bootstrap: ingested a chassis bin",
                ),
                Err(error) => tracing::warn!(
                    target: "aether_substrate::engine_server",
                    path = path_str.as_str(),
                    error = %error,
                    "binary bootstrap: skipping a bin that failed to ingest",
                ),
            }
        }
    }

    /// Ingest the component wasm at `path` into `store` content-addressed,
    /// reading its manifest straight from the wasm (ADR-0116, issue 1956) —
    /// no execution step. Returns the stored content hash, or a
    /// human-readable error for an unreadable path or an unparseable wasm.
    /// Idempotent — identical bytes dedup to the same hash.
    fn ingest_component(
        store: &mut ArtifactStore,
        path: &str,
        name: Option<String>,
    ) -> Result<String, String> {
        let bytes = fs::read(path).map_err(|e| format!("reading component path {path:?}: {e}"))?;
        let manifest = component_manifest(&bytes)
            .map_err(|e| format!("reading component manifest from {path:?}: {e}"))?;
        Ok(store.upload(
            &bytes,
            ArtifactKind::Component,
            StoredManifest::Component(manifest),
            name,
        ))
    }

    /// Resolve a [`ComponentSelector`] against `store` to its wasm bytes +
    /// manifest (ADR-0116, issue 1956). Resolution order mirrors the binary
    /// selector: an exact `query` token wins first (`hash` > `module@actor`
    /// > `name@version` (latest in v1) > `name`); absent a token, the
    /// `namespace` / `handled_kind` attribute query resolves, where a query
    /// matching more than one component is a clean ambiguity error (never a
    /// silent pick). A `module@actor` token's `@actor` part populates the
    /// reply `export` so the forwarded `LoadComponent` instantiates that
    /// actor type (ADR-0096). Returns `Err` for no match / ambiguity.
    fn resolve_component(
        store: &mut ArtifactStore,
        selector: &ComponentSelector,
    ) -> ResolveComponentResult {
        // An exact token, with the `@actor` half (if any) split off as the
        // export selector forwarded to the substrate.
        if let Some(token) = selector
            .query
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return resolve_component_token(store, token);
        }
        // No exact token: a namespace / handled-kind attribute query. A
        // match-more-than-one is a clean ambiguity error.
        let mut matches = store.list_components(&ListComponentBinaries {
            namespace: selector.namespace.clone(),
            handled_kind: selector.handled_kind,
        });
        match matches.len() {
            0 => ResolveComponentResult::Err {
                error: format!(
                    "no stored component matches the attribute query (namespace = {:?}, handled_kind = {:?})",
                    selector.namespace, selector.handled_kind,
                ),
            },
            1 => {
                let hash = matches.remove(0).hash;
                stored_component_reply(store, &hash, None)
            }
            n => ResolveComponentResult::Err {
                error: format!(
                    "the attribute query (namespace = {:?}, handled_kind = {:?}) matches {n} components — narrow it to a single component (by hash or name)",
                    selector.namespace, selector.handled_kind,
                ),
            },
        }
    }

    /// Resolve an exact component selector token to a [`ResolveComponentResult`]
    /// (ADR-0116). A `module@actor` token splits into the `module`
    /// hash/name and the `@actor` export selector; a `name@version` token
    /// is treated as `name` (latest) — v1 keeps no per-name version index;
    /// a bare token resolves as a hash first, then a name.
    fn resolve_component_token(store: &mut ArtifactStore, token: &str) -> ResolveComponentResult {
        // `module@actor` (ADR-0096) takes precedence: the `@actor` half is a
        // component `Addressable::NAMESPACE`, distinct from a binary `name@version`
        // build id. Resolve the module half (hash, then name), forward the
        // actor half as `export`.
        if let Some((module, actor)) = token.split_once('@') {
            // A hash never contains `@`, so the module half resolves as a
            // hash first, then a name (latest). The actor half is the export.
            if store.contains(module) {
                return stored_component_reply(store, module, Some(actor.to_owned()));
            }
            if let Some(found) = store.get(&Selector::Name(module.to_owned())) {
                return stored_component_reply(store, &found.hash, Some(actor.to_owned()));
            }
            return ResolveComponentResult::Err {
                error: format!("no stored component matches the selector {token:?}"),
            };
        }
        // A bare token: an exact hash wins, else a name (latest).
        if store.contains(token) {
            return stored_component_reply(store, token, None);
        }
        if let Some(found) = store.get(&Selector::Name(token.to_owned())) {
            return stored_component_reply(store, &found.hash, None);
        }
        ResolveComponentResult::Err {
            error: format!("no stored component matches the selector {token:?}"),
        }
    }

    /// Read the stored component `hash`'s wasm bytes + manifest off disk and
    /// build a `ResolveComponentResult::Ok` (ADR-0116). `export` threads a
    /// `module@actor` selector's actor half through to the forwarded
    /// `LoadComponent.export`. An entry that isn't a component (a binary
    /// hash) or whose bytes can't be read is a clean `Err`.
    fn stored_component_reply(
        store: &mut ArtifactStore,
        hash: &str,
        export: Option<String>,
    ) -> ResolveComponentResult {
        let Some(found) = store.get(&Selector::Hash(hash.to_owned())) else {
            return ResolveComponentResult::Err {
                error: format!("no stored artifact has hash {hash:?}"),
            };
        };
        let Some(manifest) = found.manifest.as_component().cloned() else {
            return ResolveComponentResult::Err {
                error: format!("artifact {hash:?} is not a component"),
            };
        };
        let wasm = match fs::read(&found.path) {
            Ok(bytes) => bytes,
            Err(e) => {
                return ResolveComponentResult::Err {
                    error: format!("reading stored component bytes for {hash:?}: {e}"),
                };
            }
        };
        ResolveComponentResult::Ok {
            hash: found.hash,
            wasm,
            name: found.name,
            manifest,
            export,
        }
    }

    /// Resolve a [`BinarySelector`] against `store` to the stored content
    /// bytes the spawn forks (ADR-0115). Resolution order: an exact `query`
    /// token wins first (`hash` > `name@version` > `name`); absent a token,
    /// the `chassis` / `caps` / `target` attribute query resolves, and with
    /// no attribute filters either, `default` = the [`DEFAULT_CHASSIS`]
    /// binary. `None` when nothing matched.
    pub(super) fn resolve_selector(
        store: &mut ArtifactStore,
        selector: &BinarySelector,
    ) -> Option<StoredArtifact> {
        if let Some(token) = selector
            .query
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            // Exact hash wins outright.
            if let Some(found) = store.get(&Selector::Hash(token.to_owned())) {
                return Some(found);
            }
            // `name@version`: the binary's self-reported build id (the
            // manifest `git_sha`) pins a specific entry of a name.
            if let Some((name, version)) = token.split_once('@') {
                let hash = pick_versioned(store, name, version)?;
                return store.get(&Selector::Hash(hash));
            }
            // A bare name points at the latest hash uploaded under it.
            return store.get(&Selector::Name(token.to_owned()));
        }
        // No exact token: an attribute query, else `default` = headless.
        let hash = store
            .list_binaries(&attribute_filter(selector))
            .into_iter()
            .map(|entry| entry.hash)
            .min()?;
        store.get(&Selector::Hash(hash))
    }

    /// The store filter for a tokenless [`BinarySelector`]: the explicit
    /// `chassis` / `caps` / `target` attribute query, or — when none is
    /// set — the `default` filter selecting the [`DEFAULT_CHASSIS`]
    /// chassis.
    fn attribute_filter(selector: &BinarySelector) -> ListEngineBinaries {
        if selector.chassis.is_none() && selector.caps.is_empty() && selector.target.is_none() {
            ListEngineBinaries {
                chassis: Some(DEFAULT_CHASSIS.to_owned()),
                caps: Vec::new(),
                target: None,
            }
        } else {
            ListEngineBinaries {
                chassis: selector.chassis.clone(),
                caps: selector.caps.clone(),
                target: selector.target.clone(),
            }
        }
    }

    /// The content hash of the entry named `name` whose manifest build id
    /// (`git_sha`) is `version` — the `name@version` selector (ADR-0115).
    /// `None` when no current entry matches both.
    fn pick_versioned(store: &ArtifactStore, name: &str, version: &str) -> Option<String> {
        store
            .list_binaries(&ListEngineBinaries::default())
            .into_iter()
            .find(|entry| entry.name.as_deref() == Some(name) && entry.manifest.git_sha == version)
            .map(|entry| entry.hash)
    }

    /// Copy the content bytes at `src` to `dest` and mark `dest`
    /// executable (`0o755` on Unix; the `from_mode` precedent in
    /// `anthropic/cli.rs`), creating `dest`'s parent dir. The
    /// realize-to-exec step for spawn: stored bytes aren't directly
    /// fork-exec'able (ADR-0115 §Execution).
    fn realize_executable(src: &Path, dest: &Path) -> io::Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dest)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    /// Push a `CallSettled::Err` back to `target` (correlation
    /// preserved) so a routed call that the cap can't satisfy — bad
    /// `engine_id`, unknown engine — closes with a wire `ReplyEnd`
    /// instead of leaving the RPC client hanging.
    fn settle_err(mailer: &Arc<Mailer>, target: MailboxId, correlation: u64, error: String) {
        mailer.push(
            Mail::new(
                target,
                <CallSettled as Kind>::ID,
                CallSettled::Err { error }.encode_into_bytes(),
                1,
            )
            .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
        );
    }

    /// Bind `127.0.0.1:0`, read the OS-assigned port, drop the
    /// listener. A tiny TOCTOU window exists before the substrate
    /// rebinds the port, but on localhost it's negligible — and this
    /// sidesteps both a wire change to report an ephemeral port back
    /// from the substrate and an un-recycled incrementing port pool.
    fn free_local_port() -> io::Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Ok(port)
    }

    /// Parent directory under which the cap allocates per-engine
    /// handle-store dirs (issue 1274). Priority:
    ///
    /// 1. `AETHER_ENGINE_STORE_ROOT` env override (ops escape hatch).
    /// 2. `dirs::data_dir().join("aether/engines")` (cross-platform
    ///    default — `~/Library/Application Support/aether/engines` on
    ///    macOS, `$XDG_DATA_HOME/aether/engines` on Linux, etc.).
    /// 3. `std::env::temp_dir().join("aether-engines")` if no data
    ///    dir is resolvable.
    // External ops escape hatch (AETHER_ENGINE_STORE_ROOT) for the per-engine
    // spawn-dir parent — the directory forked substrates and their handle
    // stores live under, resolved in a static spawn helper. #1968 deliberately
    // kept this knob inline (separate from the binary-artifact store, which it
    // moved onto EngineConfig); it is a process-level deployment override, not
    // a cap config field.
    #[allow(clippy::disallowed_methods)]
    fn engine_store_root() -> PathBuf {
        if let Ok(raw) = env::var(ENV_ENGINE_STORE_ROOT)
            && !raw.is_empty()
        {
            return PathBuf::from(raw);
        }
        if let Some(data) = dirs::data_dir() {
            return data.join("aether").join("engines");
        }
        env::temp_dir().join("aether-engines")
    }
}

// The sink's handler-signature kinds must be importable at file root
// — the `#[bridge]` macro emits `impl HandlesKind<K>` markers as
// siblings of the `sink` mod.
#[cfg(test)]
use aether_kinds::{ListEnginesResult, SpawnEngineResult, TerminateEngineResult};

/// Reply sink: records the latest reply of each engines-cap reply
/// kind into shared cells so a unit test can drive a handler via
/// `mailer.push` and observe what it replied. Lives at file root (not
/// nested in `mod tests`) so the `#[bridge]` macro's marker emission
/// stays addressable.
// `pub` (not `pub(crate)`) because it's the `NativeActor::Config` of
// the test `ReplySink` below, and the `#[actor]` macro's trait impl is
// fully public — `#[cfg(test)]` keeps it out of the real public API.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct ReplyCells {
    pub list: Arc<Mutex<Option<ListEnginesResult>>>,
    pub spawn: Arc<Mutex<Option<SpawnEngineResult>>>,
    pub terminate: Arc<Mutex<Option<TerminateEngineResult>>>,
}

#[cfg(test)]
#[aether_actor::bridge(singleton)]
mod sink {
    use super::{ListEnginesResult, ReplyCells, SpawnEngineResult, TerminateEngineResult};
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    pub struct ReplySink {
        cells: ReplyCells,
    }

    #[actor]
    impl NativeActor for ReplySink {
        type Config = ReplyCells;
        const NAMESPACE: &'static str = "aether.engine.test.reply_sink";

        fn init(cells: ReplyCells, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { cells })
        }

        #[handler]
        fn on_list_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: ListEnginesResult) {
            *self
                .cells
                .list
                .lock()
                .expect("test setup: list cell mutex poisoned") = Some(reply);
        }

        #[handler]
        fn on_spawn_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: SpawnEngineResult) {
            *self
                .cells
                .spawn
                .lock()
                .expect("test setup: spawn cell mutex poisoned") = Some(reply);
        }

        #[handler]
        fn on_terminate_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: TerminateEngineResult) {
            *self
                .cells
                .terminate
                .lock()
                .expect("test setup: terminate cell mutex poisoned") = Some(reply);
        }
    }
}

#[cfg(test)]
mod tests {
    // Test harness resolves the server/sink actor mailboxes by their NAMESPACE
    // for fixture wiring — reference id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use super::{EngineConfig, EngineServer, ReplyCells, ReplySink};
    use crate::test_chassis::TestChassis;
    use aether_actor::Addressable;
    use aether_data::{Kind, mailbox_id_from_name};
    use aether_kinds::descriptors;
    use aether_kinds::{
        BinarySelector, DeathReason, EngineAlive, EngineDied, ListEngines, SpawnEngine,
        SpawnEngineResult, TerminateEngine, TerminateEngineResult,
    };
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::mail::{Mail, Source, SourceAddr};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use std::{env, process, thread};

    /// Boot a passive chassis hosting `EngineServer` + the reply sink.
    /// Returns the chassis (kept alive for its dispatcher threads), the
    /// mailer to push requests through, and the sink's cells.
    fn boot() -> (PassiveChassis<TestChassis>, Arc<Mailer>, ReplyCells) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
        let cells = ReplyCells::default();
        // Point the cap's binary store (ADR-0115) at a per-call temp dir via
        // the ADR-0090 config field so these unit tests never touch the real
        // `dirs::data_dir()` store. Heartbeat stays disabled (the `Default`);
        // only the store dir is overridden.
        let config = EngineConfig {
            binary_store_dir: Some(isolated_store_dir()),
            ..EngineConfig::default()
        };
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<EngineServer>(config)
            .with_actor::<ReplySink>(cells.clone())
            .build_passive()
            .expect("caps boot");
        (chassis, mailer, cells)
    }

    /// A unique per-call temp dir for the engines-cap unit tests' binary
    /// store (ADR-0115), threaded onto `EngineConfig`'s `binary_store_dir`
    /// by [`boot`] so they never touch the real `dirs::data_dir()` store. No
    /// env side-channel — the store dir now rides the config (ADR-0090).
    fn isolated_store_dir() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        env::temp_dir()
            .join(format!("aether-binstore-engcap-{}-{nanos}", process::id()))
            .to_string_lossy()
            .into_owned()
    }

    /// Drive one request kind at `aether.engine`, reply-to the sink,
    /// and block until `probe` sees a recorded reply (or the deadline
    /// passes).
    fn drive<K: Kind, T>(mailer: &Arc<Mailer>, request: &K, probe: impl Fn() -> Option<T>) -> T {
        let server = mailbox_id_from_name(<EngineServer as Addressable>::NAMESPACE);
        let sink = mailbox_id_from_name(<ReplySink as Addressable>::NAMESPACE);
        mailer.push(
            Mail::new(server, K::ID, request.encode_into_bytes(), 1)
                .with_reply_to(Source::with_correlation(SourceAddr::Component(sink), 1)),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(Instant::now() < deadline, "no reply within deadline");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Push a fire-and-forget kind at the cap, then drive a `ListEngines`
    /// so the assertion runs only after the cap has processed the
    /// earlier mail (single-threaded actor, in-order mailbox). Returns
    /// the full `ListEnginesResult` the cap reports afterward — both the
    /// live `engines` and the `recently_died` ring.
    fn push_then_list<K: Kind>(
        mailer: &Arc<Mailer>,
        cells: &ReplyCells,
        fire: &K,
    ) -> aether_kinds::ListEnginesResult {
        let server = mailbox_id_from_name(<EngineServer as Addressable>::NAMESPACE);
        mailer.push(Mail::new(server, K::ID, fire.encode_into_bytes(), 1));
        drive(mailer, &ListEngines {}, || {
            cells
                .list
                .lock()
                .expect("test setup: list cell mutex poisoned")
                .take()
        })
    }

    /// `on_list` on a fresh cap replies with an empty engine list.
    #[test]
    fn list_on_empty_cap_is_empty() {
        let (_chassis, mailer, cells) = boot();
        let result = drive(&mailer, &ListEngines {}, || {
            cells
                .list
                .lock()
                .expect("test setup: list cell mutex poisoned")
                .take()
        });
        assert!(result.engines.is_empty(), "fresh cap supervises no engines");
    }

    /// `on_spawn` with a selector that resolves to no stored binary
    /// fails fast at resolution — the store is empty (each cap test
    /// isolates a fresh binary store), so no proxy is spawned and no
    /// fork is attempted (ADR-0115, #1954).
    #[test]
    fn spawn_with_missing_binary_replies_err() {
        let (_chassis, mailer, cells) = boot();
        let result = drive(
            &mailer,
            &SpawnEngine {
                selector: BinarySelector {
                    query: Some("nonexistent-hash-or-name".to_owned()),
                    chassis: None,
                    caps: vec![],
                    target: None,
                },
                args: vec![],
                boot_manifest: None,
            },
            || {
                cells
                    .spawn
                    .lock()
                    .expect("test setup: spawn cell mutex poisoned")
                    .take()
            },
        );
        match result {
            SpawnEngineResult::Err { error } => {
                assert!(
                    error.contains("no binary in the registry matched selector"),
                    "unexpected error: {error}"
                );
            }
            SpawnEngineResult::Ok { .. } => {
                panic!("an unresolvable selector must not spawn")
            }
        }
    }

    /// Bootstrap-ingest a stand-in headless binary (passed directly as
    /// the bootstrap list), then resolve the `default` selector (empty
    /// `query`, no attribute filters) to it — the bare-spawn path a
    /// fresh hub serves (ADR-0115, #1954). It forks
    /// `<stand-in> --describe`.
    #[cfg(unix)]
    #[test]
    fn bootstrap_populates_and_default_resolves_to_headless() {
        use super::server_native::{bootstrap_ingest, resolve_selector};
        use crate::store::{ArtifactStore, DEFAULT_DISK_BUDGET_BYTES};
        use std::collections::HashSet;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!(
            "aether-binstore-bootstrap-{}-{nanos}",
            process::id()
        ));
        fs::create_dir_all(&dir).expect("test setup: bootstrap temp dir");

        // A stand-in chassis bin: on `--describe` it prints a headless
        // manifest; its own bytes are what the store content-addresses.
        let stand_in = dir.join("aether-substrate-headless");
        fs::write(
            &stand_in,
            "#!/bin/sh\nif [ \"$1\" = \"--describe\" ]; then printf \
                 '{\"chassis\":\"headless\",\"caps\":[\"aether.fs\"],\"git_sha\":\"deadbee\",\
                 \"profile\":\"debug\",\"target\":\"x86_64-unknown-linux-gnu\"}'; fi\n",
        )
        .expect("test setup: write stand-in");
        fs::set_permissions(&stand_in, fs::Permissions::from_mode(0o755))
            .expect("test setup: chmod stand-in");

        let mut store = ArtifactStore::open(&dir.join("store"), DEFAULT_DISK_BUDGET_BYTES);
        let bootstrap = HashSet::from([stand_in.to_string_lossy().into_owned()]);
        bootstrap_ingest(&mut store, &bootstrap);

        let resolved = resolve_selector(
            &mut store,
            &BinarySelector {
                query: None,
                chassis: None,
                caps: vec![],
                target: None,
            },
        )
        .expect("the default selector resolves to the bootstrapped headless bin");
        assert_eq!(
            resolved
                .manifest
                .as_binary()
                .expect("the resolved artifact is a binary")
                .chassis,
            "headless",
            "default resolves to the headless chassis",
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `on_terminate` with an `engine_id` that isn't a UUID, and one
    /// that is well-formed but names no supervised engine, both reply
    /// `Err` rather than panicking.
    #[test]
    fn terminate_unknown_engine_replies_err() {
        let (_chassis, mailer, cells) = boot();

        let malformed = drive(
            &mailer,
            &TerminateEngine {
                engine_id: "not-a-uuid".to_owned(),
            },
            || {
                cells
                    .terminate
                    .lock()
                    .expect("test setup: terminate cell mutex poisoned")
                    .take()
            },
        );
        assert!(
            matches!(malformed, TerminateEngineResult::Err { .. }),
            "a malformed engine_id should be rejected",
        );

        let unknown = drive(
            &mailer,
            &TerminateEngine {
                engine_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            },
            || {
                cells
                    .terminate
                    .lock()
                    .expect("test setup: terminate cell mutex poisoned")
                    .take()
            },
        );
        assert!(
            matches!(unknown, TerminateEngineResult::Err { .. }),
            "a well-formed but unknown engine_id should be rejected",
        );
    }

    /// `on_engine_died` for an engine the cap never supervised — the
    /// terminate-race / double-report case — is an idempotent no-op,
    /// not a panic, and inserts nothing. Covers both a malformed and a
    /// well-formed-but-unknown `engine_id` (issue 1339). The
    /// `is_some()` guard also keeps the death off the recently-died
    /// ring: a `died` for an engine we never knew records no phantom
    /// death, which is what keeps the ring one-record-per-real-death
    /// under the idempotent duplicate-`died` contract (issue 1906).
    #[test]
    fn engine_died_for_unknown_is_noop() {
        let (_chassis, mailer, cells) = boot();

        let after_malformed = push_then_list(
            &mailer,
            &cells,
            &EngineDied {
                engine_id: "not-a-uuid".to_owned(),
                reason: DeathReason::Crashed {
                    detail: "peer closed".to_owned(),
                },
            },
        );
        assert!(
            after_malformed.engines.is_empty(),
            "a malformed died must not panic or insert",
        );
        assert!(
            after_malformed.recently_died.is_empty(),
            "a malformed died records no phantom death",
        );

        let after_unknown = push_then_list(
            &mailer,
            &cells,
            &EngineDied {
                engine_id: "00000000-0000-0000-0000-000000000000".to_owned(),
                reason: DeathReason::Evicted {
                    detail: "heartbeat miss limit 3 of 3".to_owned(),
                },
            },
        );
        assert!(
            after_unknown.engines.is_empty(),
            "a died for an unknown engine is a no-op",
        );
        assert!(
            after_unknown.recently_died.is_empty(),
            "a died for an unknown engine records no phantom death",
        );
    }

    /// `on_engine_alive` for an unknown engine is a silent no-op (no
    /// panic, no spurious insert) — a stale `alive` racing an eviction
    /// must not resurrect the engine (issue 1339).
    #[test]
    fn engine_alive_for_unknown_is_noop() {
        let (_chassis, mailer, cells) = boot();
        let after = push_then_list(
            &mailer,
            &cells,
            &EngineAlive {
                engine_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            },
        );
        assert!(
            after.engines.is_empty(),
            "an alive for an unknown engine must not insert it",
        );
    }
}
