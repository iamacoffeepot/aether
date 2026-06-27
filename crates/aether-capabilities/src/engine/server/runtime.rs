//! The `aether.engine` engines-cap runtime half (ADR-0122 identity/runtime
//! split). Compiled only under `feature = "runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a transport-only build of
//! the [`EngineServer`](super::EngineServer) identity never names these types
//! nor pulls `aether_substrate`. The substrate-typed imports are gated once by
//! this module rather than line-by-line; the `#[actor] impl` reaches the
//! state, ctx types, artifact/fleet helpers, and result kinds through the
//! single `use runtime::*` glob in the parent.

use super::{EngineConfig, EngineServer};
pub(super) use crate::engine::kinds::ForwardEnvelope;
use crate::engine::kinds::{EngineAlive, EngineDied, RouteEnvelope};
pub(super) use crate::engine::proxy::{EngineProxy, EngineProxyConfig, HeartbeatParams};
pub(super) use crate::engine::store::{ArtifactStore, LAYOUT_VERSION_DIR};
use aether_actor::runtime;
pub(super) use aether_data::{EngineId, Kind, MailboxId, Uuid};
pub(super) use aether_kinds::{
    DeadEngineDescriptor, DeathReason, EngineDescriptor, ListComponentBinariesResult,
    ListEngineBinariesResult, ListEnginesResult, ResolveComponentResult, SpawnEngineResult,
    TerminateEngineResult, UploadBinaryResult, UploadComponentResult,
};
use aether_kinds::{
    ListComponentBinaries, ListEngineBinaries, ListEngines, ResolveComponent, SpawnEngine,
    TerminateEngine, UploadBinary, UploadComponent,
};
pub(super) use aether_substrate::Mail;
pub(super) use aether_substrate::Subname;
pub(super) use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub(super) use aether_substrate::chassis::error::BootError;
pub(super) use aether_substrate::mail::SourceAddr;
pub(super) use aether_substrate::mail::mailer::Mailer;
pub(super) use std::collections::HashMap;
pub(super) use std::collections::VecDeque;
pub(super) use std::path::PathBuf;
pub(super) use std::process::{Command, Stdio};
pub(super) use std::sync::Arc;
pub(super) use std::time::{Duration, Instant};

// The artifact-store + fleet helpers the handlers delegate to live in the
// native-only `artifacts` / `fleet` submodules; re-export them here so the
// parent's `use runtime::*` glob reaches them alongside the rest of the
// runtime half.
pub(super) use super::artifacts::{
    bootstrap_ingest, ingest_binary, ingest_component, realize_executable, resolve_component,
    resolve_selector,
};
pub(super) use super::fleet::{engine_store_root, free_local_port, settle_err};

/// How many recently-died engines [`EngineServer`](super::EngineServer)
/// retains for `list_engines`' `recently_died` sidecar (issue 1906). A small
/// bound: the surface is "what just left and why", not an audit log —
/// the oldest record is dropped once the ring is full.
const RECENTLY_DIED_CAP: usize = 16;

/// One recently-departed engine in [`EngineServerState`]'s recently-died
/// ring (issue 1906). Cap-internal — holds the wire fields plus the
/// `Instant` the cap removed the engine, so `on_list` can compute the
/// `died_age_millis` it reports in a [`DeadEngineDescriptor`].
pub struct DeadRecord {
    pub(super) engine_id: String,
    rpc_port: u16,
    pub(super) reason: DeathReason,
    died_at: Instant,
}

/// One supervised engine in [`EngineServerState`]'s table.
pub struct EngineEntry {
    /// Mailbox of the `aether.engine.proxy:<id>` actor — the
    /// forward target for `TerminateEngine`.
    pub(super) proxy_mailbox: MailboxId,
    /// The localhost RPC port the cap assigned this substrate.
    rpc_port: u16,
    /// When the cap last saw this engine alive (issue 1339): set at
    /// spawn (just-connected = alive) and refreshed on each
    /// `EngineAlive` the proxy reports from a confirmed `Pong`.
    /// `on_list` reports `now - last_alive` as the heartbeat age.
    last_alive: Instant,
}

/// `aether.engine` runtime state (ADR-0122 split): supervises a fleet of
/// [`EngineProxy`] actors, one per spawned substrate. The addressing identity
/// is the distinct ZST [`EngineServer`](super::EngineServer); the dispatcher
/// holds this as the cap's state and routes envelopes through the
/// macro-emitted `Dispatch` impl. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without exposing
/// it as crate-public API.
pub struct EngineServerState {
    pub(super) engines: HashMap<EngineId, EngineEntry>,
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
    pub(super) mailer: Arc<Mailer>,
    /// Liveness-heartbeat tuning each spawned proxy is armed with
    /// (issue 1339), resolved once from `EngineConfig` at init.
    /// `None` disables the heartbeat fleet-wide.
    pub(super) heartbeat: Option<HeartbeatParams>,
    /// Startup-dial connect budget each spawned proxy is armed with
    /// (issue 2072), resolved once from `EngineConfig` at init.
    /// `Some(d)` caps the retry; `None` is the wait-forever sentinel.
    pub(super) connect_budget: Option<Duration>,
    /// Bounded ring of the last [`RECENTLY_DIED_CAP`] engines that
    /// left the table and why (issue 1906). `on_terminate` /
    /// `on_engine_died` push a [`DeadRecord`] at the removal site;
    /// `on_list` renders it into the reply's `recently_died` sidecar
    /// so an observer can tell a clean terminate from a crash or a
    /// heartbeat eviction.
    pub(super) recently_died: VecDeque<DeadRecord>,
    /// Hub-scoped content-addressed binary store (ADR-0115, issue
    /// 1953) — the storage half of the artifact registry.
    /// `on_upload_binary` ingests a staged binary content-addressed;
    /// `on_list_engine_binaries` enumerates the stored entries. Built from
    /// `EngineConfig` (the layout dir + disk budget) at init so it
    /// persists across a `restart-hub` (the layout root outlives the
    /// hub child); the spawn cutover (#1954) reads it back through the
    /// store's `get` seam.
    pub(super) store: ArtifactStore,
}

impl EngineServerState {
    /// Push a [`DeadRecord`] onto the recently-died ring, evicting the
    /// oldest entry once the ring is full (issue 1906).
    pub(super) fn record_death(&mut self, engine_id: String, rpc_port: u16, reason: DeathReason) {
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
}

#[runtime]
impl NativeActor for EngineServer {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// supervised-fleet table plus the content-addressed artifact store.
    type State = EngineServerState;
    type Config = EngineConfig;
    const NAMESPACE: &'static str = "aether.engine";

    fn init(
        config: EngineConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<EngineServerState, BootError> {
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
        Ok(EngineServerState {
            engines: HashMap::new(),
            next_engine_seq: 1,
            mailer: ctx.mailer(),
            heartbeat: config.heartbeat_params(),
            connect_budget: config.connect_budget(),
            recently_died: VecDeque::new(),
            store,
        })
    }

    /// Enumerate every engine the cap currently supervises.
    ///
    /// # Agent
    /// Send `ListEngines` (fieldless). Reply: `ListEnginesResult
    /// { engines: [{ engine_id, rpc_port, last_heartbeat_age_millis }],
    /// recently_died: [{ engine_id, rpc_port, reason, died_age_millis }] }`.
    #[handler]
    fn on_list(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: ListEngines,
    ) -> ListEnginesResult {
        let now = Instant::now();
        let engines = state
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
        let recently_died = state
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
    fn on_spawn(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SpawnEngine,
    ) -> SpawnEngineResult {
        // Resolve the registry selector to stored content bytes before
        // any side effect, so a miss returns without reserving a port
        // or burning an engine id (ADR-0115, #1954).
        let Some(artifact) = resolve_selector(&mut state.store, &mail.selector) else {
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
        let engine_id = EngineId(Uuid::from_u128(state.next_engine_seq));
        state.next_engine_seq += 1;
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
                    heartbeat: state.heartbeat,
                    connect_budget: state.connect_budget,
                },
            )
            .finish();

        match result {
            Ok(proxy_mailbox) => {
                state.engines.insert(
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
        state: &mut Self::State,
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

        let Some(entry) = state.engines.remove(&engine_id) else {
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
        state.record_death(
            mail.engine_id.clone(),
            entry.rpc_port,
            DeathReason::Terminated,
        );

        // Forward to the proxy: it SIGKILLs its substrate and
        // self-shuts-down. Fire-and-forget — the proxy doesn't
        // reply, and the table entry is already gone, so the
        // returned MailId has nothing to subscribe against.
        let payload = mail.encode_into_bytes();
        let _ = ctx.send_envelope_traced(proxy_mailbox, <TerminateEngine as Kind>::ID, &payload);
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
    fn on_route(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: RouteEnvelope) {
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
                    &state.mailer,
                    reply_target,
                    correlation,
                    format!("engine_id {:?} is not a valid UUID: {e}", mail.engine_id),
                );
                return;
            }
        };
        let Some(entry) = state.engines.get(&engine_id) else {
            settle_err(
                &state.mailer,
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
        state.mailer.push(
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
    fn on_engine_died(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: EngineDied) {
        let Ok(uuid) = Uuid::parse_str(&mail.engine_id) else {
            tracing::warn!(
                target: "aether_substrate::engine_server",
                engine_id = %mail.engine_id,
                "engine died: unparseable engine_id; ignoring",
            );
            return;
        };
        if let Some(entry) = state.engines.remove(&EngineId(uuid)) {
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
            state.record_death(mail.engine_id, rpc_port, mail.reason);
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
    fn on_engine_alive(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: EngineAlive) {
        let Ok(uuid) = Uuid::parse_str(&mail.engine_id) else {
            return;
        };
        if let Some(entry) = state.engines.get_mut(&EngineId(uuid)) {
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
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: UploadBinary,
    ) -> UploadBinaryResult {
        match ingest_binary(&mut state.store, &mail.staged_path, mail.name.clone()) {
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
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ListEngineBinaries,
    ) -> ListEngineBinariesResult {
        ListEngineBinariesResult {
            binaries: state.store.list_binaries(&mail),
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
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: UploadComponent,
    ) -> UploadComponentResult {
        match ingest_component(&mut state.store, &mail.staged_path, mail.name.clone()) {
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
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ResolveComponent,
    ) -> ResolveComponentResult {
        resolve_component(&mut state.store, &mail.selector)
    }

    /// Enumerate the hub's stored component binaries.
    ///
    /// # Agent
    /// Send `ListComponentBinaries { namespace?, handled_kind? }` (each
    /// filter AND-combined; an absent field is no constraint). Reply:
    /// `ListComponentBinariesResult { components: [{ hash, name, manifest }] }`.
    #[handler]
    fn on_list_component_binaries(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ListComponentBinaries,
    ) -> ListComponentBinariesResult {
        ListComponentBinariesResult {
            components: state.store.list_components(&mail),
        }
    }
}
