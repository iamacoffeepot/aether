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
use crate::engine::kinds::{EngineAlive, EngineDied, RouteEnvelope};
use aether_kinds::{
    ListComponentBinaries, ListEngineBinaries, ListEngines, ResolveComponent, SpawnEngine,
    TerminateEngine, UploadBinary, UploadComponent,
};
#[cfg(test)]
use std::sync::{Arc, Mutex};

// The engines cap's implementation, split along its seams (ADR-0121):
// `config` (the ADR-0090 config struct + parsers), `artifacts` (the
// content-addressed store resolution / ingestion the handlers delegate
// to), and `fleet` (free-port allocation, routed-call settlement, and
// spawn-dir resolution). All three are native-only — the cap forks
// processes and owns sockets — so they elide on wasm alongside the
// bridge mod.
#[cfg(not(target_arch = "wasm32"))]
mod artifacts;
#[cfg(not(target_arch = "wasm32"))]
mod config;
#[cfg(not(target_arch = "wasm32"))]
mod fleet;

// `EngineConfig` (+ its derive-emitted `EngineOverlay`) ride through
// file root for the hub chassis bin, which flattens the overlay into
// `HubCli`, resolves argv-then-env, and passes the config to
// `with_actor::<EngineServer>(cfg)` (ADR-0090). Native-only re-export —
// the engines cap is native-only, so the config has no wasm consumer.
#[cfg(not(target_arch = "wasm32"))]
pub use config::{EngineConfig, EngineConfigLayer, EngineOverlay};

#[aether_actor::bridge(singleton)]
mod server_native {
    use super::artifacts::{
        bootstrap_ingest, ingest_binary, ingest_component, realize_executable, resolve_component,
        resolve_selector,
    };
    use super::config::EngineConfig;
    use super::fleet::{engine_store_root, free_local_port, settle_err};
    use super::{
        EngineAlive, EngineDied, ListComponentBinaries, ListEngineBinaries, ListEngines,
        ResolveComponent, RouteEnvelope, SpawnEngine, TerminateEngine, UploadBinary,
        UploadComponent,
    };
    use crate::engine::kinds::ForwardEnvelope;
    use crate::engine::proxy::{EngineProxy, EngineProxyConfig, HeartbeatParams};
    use crate::store::{ArtifactStore, LAYOUT_VERSION_DIR};
    use aether_actor::actor;
    use aether_data::{EngineId, Kind, MailboxId, Uuid};
    use aether_kinds::{
        DeadEngineDescriptor, DeathReason, EngineDescriptor, ListComponentBinariesResult,
        ListEngineBinariesResult, ListEnginesResult, ResolveComponentResult, SpawnEngineResult,
        TerminateEngineResult, UploadBinaryResult, UploadComponentResult,
    };
    use aether_substrate::Mail;
    use aether_substrate::Subname;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::SourceAddr;
    use aether_substrate::mail::mailer::Mailer;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

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
    use crate::engine::kinds::{EngineAlive, EngineDied};
    use crate::test_chassis::TestChassis;
    use aether_actor::Addressable;
    use aether_data::{Kind, mailbox_id_from_name};
    use aether_kinds::descriptors;
    use aether_kinds::{
        BinarySelector, DeathReason, ListEngines, SpawnEngine, SpawnEngineResult, TerminateEngine,
        TerminateEngineResult,
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
        use super::artifacts::{bootstrap_ingest, resolve_selector};
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
