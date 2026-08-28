//! The `aether.fleet` engines-cap runtime half (ADR-0122 identity/runtime
//! split). The [`FleetServer`] identity file names none
//! of these types. The substrate-typed imports are collected once by
//! this module rather than line-by-line; the `#[actor] impl` reaches the
//! state, ctx types, artifact/fleet helpers, and result kinds through the
//! single `use runtime::*` glob in the parent.

use super::config::RestartPolicy;
use super::restart::schedule_restart;
use super::{FleetConfig, FleetServer};
use crate::child_env::isolate_child_environment;
pub use crate::kinds::ForwardEnvelope;
use crate::kinds::{EngineAlive, EngineDied, EngineRestartDue};
pub use crate::proxy::{FleetProxy, FleetProxyConfig, HeartbeatParams, is_reforkable_spawn_failure};
pub use crate::store::{ArtifactStore, LAYOUT_VERSION_DIR};
use aether_actor::runtime;
pub use aether_actor::{Manual, Single};
pub use aether_data::{EngineId, Kind, MailboxId, Uuid};
use aether_kinds::{
    BinarySelector, ListComponentBinaries, ListEngineBinaries, ListEngines, ResolveComponent, SpawnEngine,
    TerminateEngine, UploadBinary, UploadComponent,
};
pub use aether_kinds::{
    DeadEngineDescriptor, DeathReason, EngineDescriptor, ListComponentBinariesResult, ListEngineBinariesResult,
    ListEnginesResult, ResolveComponentResult, SpawnEngineResult, TerminateEngineResult, UploadBinaryResult,
    UploadComponentResult,
};
use aether_rpc::RouteEnvelope;
pub use aether_substrate::Mail;
pub use aether_substrate::Subname;
pub use aether_substrate::actor::native::{
    DeferredReply, NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone,
};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::SourceAddr;
pub use aether_substrate::mail::mailer::Mailer;
pub use std::collections::HashMap;
pub use std::collections::VecDeque;
pub use std::path::{Path, PathBuf};
pub use std::process::{Child, Command, Stdio};
pub use std::sync::Arc;
pub use std::time::{Duration, Instant};

// The artifact-store + fleet helpers the handlers delegate to live in the
// native-only `artifacts` / `fleet` submodules; re-export them here so the
// parent's `use runtime::*` glob reaches them alongside the rest of the
// runtime half.
pub use super::artifacts::{
    bootstrap_ingest, ingest_binary, ingest_component, realize_executable, resolve_component, resolve_selector,
};
pub use super::fleet::{free_local_port, resolve_fleet_store_root, settle_err};

/// How many recently-died engines [`FleetServer`]
/// retains for `list_engines`' `recently_died` sidecar (issue 1906). A small
/// bound: the surface is "what just left and why", not an audit log —
/// the oldest record is dropped once the ring is full.
const RECENTLY_DIED_CAP: usize = 16;

/// One recently-departed engine in [`FleetServerState`]'s recently-died
/// ring (issue 1906). Cap-internal — holds the wire fields plus the
/// `Instant` the cap removed the engine, so `on_list` can compute the
/// `died_age_millis` it reports in a [`DeadEngineDescriptor`].
pub struct DeadRecord {
    pub engine_id: String,
    pub rpc_port: u16,
    pub reason: DeathReason,
    pub died_at: Instant,
}

/// Everything needed to fork one substrate again exactly as it was
/// forked the first time.
///
/// Retained on a supervised engine so an automatic restart re-runs the
/// original recipe rather than a reconstruction of it. The selector is
/// kept **resolved**, as the content hash `resolve_selector` returned —
/// not the caller's `BinarySelector`. A bare name or attribute query
/// resolves against whatever the store holds *now*, so replaying it could
/// silently restart a crashed engine onto a different binary than the one
/// that crashed; the hash cannot. The hash is still re-resolved at
/// restart time, because the store's LRU may have evicted it since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRecipe {
    /// Content hash of the binary this engine was forked from.
    pub hash: String,
    /// The caller's per-spawn argv, forwarded verbatim ahead of the
    /// hub's own injections.
    pub args: Vec<String>,
    /// Boot-manifest path, when the spawn carried a component list.
    pub boot_manifest: Option<String>,
}

/// Restart bookkeeping for one engine *lineage* — the chain of engine
/// ids a repeatedly-restarted engine passes through.
///
/// A restarted engine gets a fresh id (identity continuity across a
/// restart is deliberately out of scope), so a burst budget keyed on
/// engine id would reset on every restart and never bind. Carrying the
/// ledger forward through the successor's spawn context is what makes
/// the limit hold across the whole lineage.
#[derive(Clone, Debug)]
pub struct Supervision {
    /// How to fork this engine again.
    pub recipe: SpawnRecipe,
    /// When each automatic restart of this lineage was admitted,
    /// oldest first. Entries are pruned as they age out of the policy
    /// window, so the length is the budget spent right now.
    pub restarts: VecDeque<Instant>,
}

impl Supervision {
    /// Open a ledger for an engine spawned by request. A spawn the
    /// operator asked for is not a restart, so the budget starts full.
    pub fn new(recipe: SpawnRecipe) -> Self {
        Self { recipe, restarts: VecDeque::new() }
    }

    /// Admit one restart against the burst budget, reporting whether it
    /// may proceed. Ages spent restarts out of the window first, so a
    /// lineage that crashes rarely is always recovered while one
    /// crash-looping exhausts its budget and stays dead.
    ///
    /// Charges the budget only when it admits: a refusal must not push
    /// the window forward, or a caller that kept asking would hold the
    /// engine down indefinitely past the point the window had cleared.
    pub fn admit_restart(&mut self, policy: RestartPolicy, now: Instant) -> bool {
        while self.restarts.front().is_some_and(|at| now.duration_since(*at) >= policy.burst_window) {
            self.restarts.pop_front();
        }

        let admitted = self.restarts.len() < policy.burst_limit as usize;
        if admitted {
            self.restarts.push_back(now);
        }
        admitted
    }
}

/// Whether a [`DeathReason`] is one automatic restart supervision acts
/// on.
///
/// `Terminated` never is: the operator asked for that engine to be gone,
/// and an engines cap that forked it straight back would make
/// `terminate_substrate` unable to do the one thing it exists for.
/// `SpawnFailed` never is either — it names a spawn that failed, so
/// there is no supervised engine to recover and `on_spawn`'s own bounded
/// re-fork already owns that retry.
pub fn restart_applies_to(reason: &DeathReason) -> bool {
    matches!(reason, DeathReason::Crashed { .. } | DeathReason::Evicted { .. })
}

/// The complete argv tail a substrate is forked with, for one recipe on
/// one port.
///
/// argv is the machine channel (ADR-0162: config is addressed via argv,
/// never ambient env). The caller's per-spawn `args` go first, then the
/// hub's own injections as the child's derive-emitted overlay flags
/// (ADR-0156): `--rpc-port` assigns the substrate's RPC bind port, and a
/// spawn carrying a component list rides `--boot-manifest` so the chassis
/// reads the listed wasm itself (issue 1776). A binary lacking these
/// flags fails at spawn.
///
/// Built here rather than inline at the fork so the requested-spawn and
/// restart paths provably construct the same command line — the drift
/// this exists to prevent is a restart that quietly loses the caller's
/// args or its boot manifest.
pub fn spawn_args(recipe: &SpawnRecipe, rpc_port: u16) -> Vec<String> {
    let mut args = recipe.args.clone();
    args.push("--rpc-port".to_owned());
    args.push(rpc_port.to_string());
    if let Some(boot_manifest) = &recipe.boot_manifest {
        args.push("--boot-manifest".to_owned());
        args.push(boot_manifest.clone());
    }
    args
}

/// The exact-hash selector a restart re-resolves its recipe through.
/// `resolve_selector` tries `Selector::Hash` before any name or attribute
/// interpretation, so a content hash can only ever resolve to the content
/// it names.
fn hash_selector(hash: &str) -> BinarySelector {
    BinarySelector { query: Some(hash.to_owned()), chassis: None, caps: Vec::new(), target: None }
}

/// Fork the substrate into its own process group, so the proxy that owns
/// it can signal the whole group at teardown.
///
/// Without this the substrate shares the hub's group, and anything the
/// substrate forks is reachable only through the substrate's own
/// shutdown — a wedged or killed substrate would orphan its children onto
/// init. On non-unix there are no process groups to leave and the bare
/// kill is already the whole subtree.
fn set_own_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// One supervised engine in [`FleetServerState`]'s table.
pub struct EngineEntry {
    /// Mailbox of the `aether.fleet.proxy:<id>` actor — the
    /// forward target for `TerminateEngine`.
    pub proxy_mailbox: MailboxId,
    /// The localhost RPC port the cap assigned this substrate.
    pub rpc_port: u16,
    /// When the cap last saw this engine alive (issue 1339): set at
    /// spawn (just-connected = alive) and refreshed on each
    /// `EngineAlive` the proxy reports from a confirmed `Pong`.
    /// `on_list` reports `now - last_alive` as the heartbeat age.
    pub last_alive: Instant,
    /// The recipe to re-fork this engine from, plus the restart budget
    /// its lineage has already spent. Retained whether or not restart
    /// supervision is armed — the cost is one small struct per engine,
    /// and carrying it unconditionally keeps the spawn path free of a
    /// policy branch that would otherwise decide, at fork time, whether
    /// a later recovery is even possible.
    pub supervision: Supervision,
}

/// Actor-local bookkeeping for a proxy whose initialized state has been
/// staged but whose route is not authoritatively Live yet. Pending engines
/// are deliberately absent from [`FleetServerState::engines`], so list,
/// route, and terminate cannot observe a reservation as a supervised engine.
pub struct PendingEngine {
    pub rpc_port: u16,
    /// A Live proxy can report its death from the activation catch-up wake
    /// before the parent's later task completion runs. Latch only the first
    /// report so completion cannot install a corpse or duplicate its death.
    pub early_death: Option<DeathReason>,
}

/// Context carried by the staged proxy birth into its authoritative task
/// completion. Process ownership stays solely in `FleetProxyState`; the proxy's
/// own identity rides its `SpawnOutcome`. What this carries is the fleet
/// metadata no spawn result knows: the engine id the cap minted and the RPC
/// port it reserved for the forked substrate.
#[derive(Clone)]
pub struct FleetSpawnContext {
    pub engine_id: EngineId,
    pub rpc_port: u16,
    /// The recipe + restart ledger to install on the engine this birth
    /// commits. For a restart this is the dead engine's ledger carried
    /// forward, which is what makes the burst limit bind across a
    /// lineage whose engine id changes on every restart.
    pub supervision: Supervision,
    /// Who ordered this birth. Decides only what its completion owes:
    /// a requested spawn owes the caller a `SpawnEngineResult`, a
    /// restart owes nobody and must discharge without replying.
    pub origin: SpawnOrigin,
}

/// What ordered a proxy birth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnOrigin {
    /// A `SpawnEngine` from a caller who is waiting on the reply.
    Requested,
    /// Automatic restart supervision re-forking a dead engine. There is
    /// no deferred reply behind it, so its completion releases the
    /// settlement hold without sending one.
    Restarted,
}

/// One prepared-but-not-yet-supervised substrate: the port reserved, the
/// id minted, the binary realized, and the process forked. Both the
/// requested-spawn and the restart path build this the same way through
/// [`FleetServerState::prepare_fork`], so neither can drift from the
/// other on argv, environment, or process-group construction.
pub struct PreparedFork {
    pub engine_id: EngineId,
    pub rpc_port: u16,
    pub rpc_addr: String,
    pub child: Child,
}

/// Why a [`FleetServerState::prepare_fork`] did not produce a child.
///
/// The split is about what the caller owes, not about severity: a
/// failure before an engine id was minted has nothing to correlate or
/// reap, while one after leaves an id that must reach the caller and the
/// recently-died ring (issue 2423).
pub enum PrepareFailure {
    /// Failed before minting an engine id — nothing to record.
    PreAllocation(String),
    /// Failed after minting `engine_id`, so the caller records a
    /// `SpawnFailed` death against it and hands the id back.
    PostAllocation { engine_id: EngineId, rpc_port: u16, error: String },
}

pub enum ProxySpawnOutcome {
    Applied(MailboxId),
    Rejected(String),
}

pub enum EngineDeathDisposition {
    PendingLatched,
    PendingDuplicate,
    /// The engine was supervised and has been evicted and recorded. Its
    /// retained [`Supervision`] rides out so the caller can decide
    /// whether to restart the lineage — the entry itself is gone, so
    /// this is the only remaining copy of the recipe.
    LiveRemoved(Box<Supervision>),
    Unknown,
}

/// `aether.fleet` runtime state (ADR-0122 split): supervises a fleet of
/// [`FleetProxy`] actors, one per spawned substrate. The addressing identity
/// is the distinct ZST [`FleetServer`]; the dispatcher
/// holds this as the cap's state and routes envelopes through the
/// macro-emitted `Dispatch` impl. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without exposing
/// it as crate-public API.
pub struct FleetServerState {
    pub engines: HashMap<EngineId, EngineEntry>,
    /// Initialized proxy births awaiting authoritative owner settlement.
    /// Entries never participate in the public fleet surfaces.
    pub pending_engines: HashMap<EngineId, PendingEngine>,
    /// Monotonic source of `EngineId`s. Engine ids only need to be
    /// unique among the engines this cap currently supervises — a
    /// process-local counter delivers that without a `uuid` rng
    /// dependency. Starts at 1 (`Uuid::from_u128(0)` is the nil
    /// uuid).
    pub next_engine_seq: u128,
    /// Cached so `on_route` can push a `ForwardEnvelope` at a proxy
    /// while *propagating the inbound reply-to* — `NativeCtx`'s
    /// sends stamp the cap as sender, but a routed call's reply
    /// must reach the originating `RpcServerCapability`, not here.
    pub mailer: Arc<Mailer>,
    /// Liveness-heartbeat tuning each spawned proxy is armed with
    /// (issue 1339), resolved once from `FleetConfig` at init.
    /// `None` disables the heartbeat fleet-wide.
    pub heartbeat: Option<HeartbeatParams>,
    /// Startup-dial connect budget each spawned proxy is armed with
    /// (issue 2072), resolved once from `FleetConfig` at init.
    /// `Some(d)` caps the retry; `None` is the wait-forever sentinel.
    pub connect_budget: Option<Duration>,
    /// How many times `on_spawn` re-forks a substrate on a fresh port
    /// before giving up (issue 2422), resolved once from `FleetConfig`
    /// at init. A freshly-forked substrate can lose its guessed RPC
    /// port to another socket in `free_local_port`'s TOCTOU window and
    /// exit on a fatal bind; a re-fork on a fresh port escapes it.
    /// Clamped to at least 1.
    pub spawn_attempts: u32,
    /// Parent directory under which the cap allocates per-engine
    /// spawn / handle-store dirs (issue 1274), resolved once from
    /// `FleetConfig::fleet_store_root` at init via
    /// [`resolve_fleet_store_root`]. `on_spawn` joins each freshly
    /// minted `engine_id` onto this to get the engine's scratch dir.
    pub fleet_store_root: PathBuf,
    /// Bounded ring of the last [`RECENTLY_DIED_CAP`] engines that
    /// left the table and why (issue 1906). `on_terminate` /
    /// `on_engine_died` push a [`DeadRecord`] at the removal site;
    /// `on_list` renders it into the reply's `recently_died` sidecar
    /// so an observer can tell a clean terminate from a crash or a
    /// heartbeat eviction.
    pub recently_died: VecDeque<DeadRecord>,
    /// Hub-scoped content-addressed binary store (ADR-0115, issue
    /// 1953) — the storage half of the artifact registry.
    /// `on_upload_binary` ingests a staged binary content-addressed;
    /// `on_list_engine_binaries` enumerates the stored entries. Built from
    /// `FleetConfig` (the layout dir + disk budget) at init so it
    /// persists across a `restart-hub` (the layout root outlives the
    /// hub child); the spawn cutover (#1954) reads it back through the
    /// store's `get` seam.
    pub store: ArtifactStore,
    /// This cap's own mailbox, retained so a restart-backoff timer has
    /// somewhere to fire its [`EngineRestartDue`] wake.
    pub self_mailbox: MailboxId,
    /// The automatic-restart policy, or `None` when a dead engine stays
    /// dead. Resolved once from `FleetConfig` at init.
    pub restart_policy: Option<RestartPolicy>,
    /// Restarts whose backoff is still running, keyed by the token their
    /// timer will fire back. Holding the recipe here rather than on the
    /// timer keeps [`EngineRestartDue`] a bare alarm — see
    /// [`super::restart`].
    pub pending_restarts: HashMap<u64, Supervision>,
    /// Monotonic source of restart tokens. Process-local and never
    /// externally addressable, so a plain counter is enough.
    pub next_restart_token: u64,
}

impl FleetServerState {
    /// Push a [`DeadRecord`] onto the recently-died ring, evicting the
    /// oldest entry once the ring is full (issue 1906).
    pub fn record_death(&mut self, engine_id: String, rpc_port: u16, reason: DeathReason) {
        if self.recently_died.len() >= RECENTLY_DIED_CAP {
            self.recently_died.pop_front();
        }
        self.recently_died.push_back(DeadRecord { engine_id, rpc_port, reason, died_at: Instant::now() });
    }

    /// Record a post-allocation spawn failure (issue 2423): write a
    /// `SpawnFailed` death keyed by `engine_id` to the recently-died ring
    /// and return the id-bearing `Err` so a caller can correlate and
    /// reap. For the failures after an `engine_id` has been minted but
    /// before the engine was ever registered alive.
    fn fail_spawn(&mut self, engine_id: EngineId, rpc_port: u16, error: String) -> SpawnEngineResult {
        self.record_death(engine_id.0.to_string(), rpc_port, DeathReason::SpawnFailed { detail: error.clone() });
        SpawnEngineResult::Err { engine_id: Some(engine_id.0.to_string()), error }
    }

    /// Decide whether a just-evicted engine should be restarted, and if
    /// so file its recipe and start the backoff timer.
    ///
    /// The death is already recorded by the time this runs, so every exit
    /// here is a complete, honest outcome — a refusal leaves exactly the
    /// state the cap had before restart supervision existed.
    ///
    /// Returns whether a restart was scheduled, so a caller (and a test)
    /// can distinguish "declined" from "under way" without inspecting the
    /// timer.
    pub fn consider_restart(&mut self, engine_id: &str, reason: &DeathReason, mut supervision: Supervision) -> bool {
        let Some(policy) = self.restart_policy else {
            return false;
        };
        if !restart_applies_to(reason) {
            return false;
        }

        if !supervision.admit_restart(policy, Instant::now()) {
            // Loud, not quiet: an engine the cap has given up on is an
            // operator-visible event, and the burst numbers are what
            // explain why recovery stopped.
            tracing::error!(
                target: "aether_substrate::fleet_server",
                engine_id = %engine_id,
                reason = ?reason,
                burst_limit = policy.burst_limit,
                burst_window_secs = policy.burst_window.as_secs(),
                "engine restart: burst limit exhausted; giving up on this engine",
            );
            return false;
        }

        let token = self.next_restart_token;
        self.next_restart_token += 1;
        self.pending_restarts.insert(token, supervision);
        tracing::warn!(
            target: "aether_substrate::fleet_server",
            engine_id = %engine_id,
            reason = ?reason,
            backoff_millis = u64::try_from(policy.backoff.as_millis()).unwrap_or(u64::MAX),
            "engine restart: scheduling a re-fork after the backoff",
        );
        schedule_restart(&self.mailer, self.self_mailbox, token, policy.backoff);
        true
    }

    /// Reserve a port, mint an engine id, realize the binary, and fork it
    /// — everything a substrate needs before a proxy can be pointed at
    /// it, and nothing about who is waiting for the result.
    ///
    /// The one fork site. `on_spawn` and the restart path differ only in
    /// what they owe their caller and how they stage the proxy; routing
    /// both through here is what keeps a restarted engine's argv,
    /// environment, and process group identical to the spawn it is
    /// recovering rather than a second implementation that drifts.
    fn prepare_fork(&mut self, exec_source: &Path, recipe: &SpawnRecipe) -> Result<PreparedFork, PrepareFailure> {
        let rpc_port = free_local_port()
            .map_err(|e| PrepareFailure::PreAllocation(format!("could not allocate an RPC port: {e}")))?;

        let engine_id = EngineId(Uuid::from_u128(self.next_engine_seq));
        self.next_engine_seq += 1;
        let post = |error| PrepareFailure::PostAllocation { engine_id, rpc_port, error };

        // Stored bytes are content-addressed and not directly
        // fork-exec'able, so materialize the resolved entry to an
        // executable temp file under this engine's own scratch dir and
        // fork that (ADR-0115 §Execution); the caller never sees the path.
        let exec_path = self.fleet_store_root.join(engine_id.0.simple().to_string()).join("substrate");
        realize_executable(exec_source, &exec_path)
            .map_err(|e| post(format!("materializing binary {} to {}: {e}", recipe.hash, exec_path.display())))?;

        let mut command = Command::new(&exec_path);
        command.stdin(Stdio::null());
        // ADR-0162: a spawned engine's environment is constructed, never
        // inherited. Clear it and copy only the platform/third-party
        // allowlist (locale, proxy, driver vars, `PATH` / `HOME`, …); no
        // `AETHER_*` key survives, so aether config can't ride the ambient
        // channel. The child's addressed config rides argv below, and
        // argv does not inherit — so a substrate that forks its own
        // subprocess isolates by construction a generation down.
        isolate_child_environment(&mut command);
        command.args(spawn_args(recipe, rpc_port));
        set_own_process_group(&mut command);

        let child = command.spawn().map_err(|e| post(format!("failed to spawn {}: {e}", exec_path.display())))?;

        Ok(PreparedFork { engine_id, rpc_port, rpc_addr: format!("127.0.0.1:{rpc_port}"), child })
    }

    /// Re-fork a dead engine from the recipe it was spawned with, and
    /// stage a fresh proxy over it.
    ///
    /// The successor gets a **new** engine id: identity continuity across
    /// a restart is deliberately out of scope here, and a caller holding
    /// the old id learns the engine is gone from the recently-died ring
    /// the way it always has. What *is* continuous is `supervision` — the
    /// recipe and the spent restart budget ride across, so the burst
    /// limit binds over the lineage rather than resetting on every new id.
    fn restart_engine(&mut self, ctx: &mut NativeCtx<'_, Single, FleetServer>, supervision: Supervision) {
        let hash = supervision.recipe.hash.clone();

        // Re-resolve rather than trusting a path captured at spawn time:
        // the store's LRU may have evicted this content since, in which
        // case the recipe is no longer runnable and the engine simply
        // stays dead. Its death is already recorded — there is no engine
        // id for this attempt, so there is nothing honest to key a second
        // record on.
        let Some(artifact) = resolve_selector(&mut self.store, &hash_selector(&hash)) else {
            tracing::error!(
                target: "aether_substrate::fleet_server",
                hash = %hash,
                "engine restart: the recipe's binary is no longer in the store; the engine stays dead",
            );
            return;
        };

        let prepared = match self.prepare_fork(&artifact.path, &supervision.recipe) {
            Ok(prepared) => prepared,
            Err(failure) => {
                let (engine_id, rpc_port, error) = match failure {
                    PrepareFailure::PreAllocation(error) => {
                        tracing::error!(
                            target: "aether_substrate::fleet_server",
                            hash = %hash,
                            error = %error,
                            "engine restart: could not prepare the re-fork; the engine stays dead",
                        );
                        return;
                    }
                    PrepareFailure::PostAllocation { engine_id, rpc_port, error } => (engine_id, rpc_port, error),
                };
                // An id was minted, so the failed recovery is
                // correlatable: record it the way a failed spawn is
                // (issue 2423) rather than only logging.
                tracing::error!(
                    target: "aether_substrate::fleet_server",
                    engine_id = %engine_id.0,
                    error = %error,
                    "engine restart: the re-fork failed; the engine stays dead",
                );
                self.record_death(engine_id.0.to_string(), rpc_port, DeathReason::SpawnFailed { detail: error });
                return;
            }
        };

        let PreparedFork { engine_id, rpc_port, rpc_addr, child } = prepared;
        let subname = engine_id.0.simple().to_string();
        let staged = ctx
            .spawn_child::<FleetProxy>(
                Subname::Named(&subname),
                FleetProxyConfig {
                    engine_id,
                    rpc_addr,
                    spawned: Some(child),
                    heartbeat: self.heartbeat,
                    connect_budget: self.connect_budget,
                },
                (),
            )
            .stage_with(FleetSpawnContext { engine_id, rpc_port, supervision, origin: SpawnOrigin::Restarted });

        match staged {
            Ok(_) => {
                let replaced = self.pending_engines.insert(engine_id, PendingEngine { rpc_port, early_death: None });
                debug_assert!(replaced.is_none(), "fresh engine ids cannot replace a pending spawn");
                tracing::warn!(
                    target: "aether_substrate::fleet_server",
                    engine_id = %engine_id.0,
                    rpc_port,
                    "engine restart: re-forked a dead engine under a fresh id",
                );
            }
            Err(e) => {
                // The staging itself was rejected, so no completion is
                // coming and `FleetProxyState` never took ownership of
                // the child — except on the init-failure path, which
                // already terminated its group. Record the failed
                // recovery against the id it burned.
                let error = format!("proxy failed to connect to the restarted substrate: {e:?}");
                tracing::error!(
                    target: "aether_substrate::fleet_server",
                    engine_id = %engine_id.0,
                    error = %error,
                    "engine restart: the replacement proxy did not come up; the engine stays dead",
                );
                self.record_death(engine_id.0.to_string(), rpc_port, DeathReason::SpawnFailed { detail: error });
            }
        }
    }

    /// Apply the actor-local half of a staged proxy settlement. Returning
    /// `None` suppresses a stale completion; the binding-owned task ledger
    /// still gets discharged by the caller without emitting a second reply.
    pub fn settle_pending_spawn(
        &mut self,
        spawn: FleetSpawnContext,
        outcome: ProxySpawnOutcome,
    ) -> Option<SpawnEngineResult> {
        let FleetSpawnContext { engine_id, rpc_port, supervision, .. } = spawn;
        let pending = self.pending_engines.remove(&engine_id)?;
        debug_assert_eq!(pending.rpc_port, rpc_port, "spawn completion must match its pending engine");

        Some(match outcome {
            ProxySpawnOutcome::Rejected(error) => self.fail_spawn(engine_id, rpc_port, error),
            ProxySpawnOutcome::Applied(proxy_mailbox) => {
                if let Some(reason) = pending.early_death {
                    let error = format!("proxy died before supervision committed: {reason:?}");
                    self.record_death(engine_id.0.to_string(), rpc_port, reason);
                    SpawnEngineResult::Err { engine_id: Some(engine_id.0.to_string()), error }
                } else {
                    self.engines.insert(
                        engine_id,
                        EngineEntry {
                            proxy_mailbox,
                            rpc_port,
                            // Authoritative activation + no early death =
                            // alive at the supervision commit boundary.
                            last_alive: Instant::now(),
                            supervision,
                        },
                    );
                    SpawnEngineResult::Ok { engine_id: engine_id.0.to_string(), rpc_port }
                }
            }
        })
    }

    /// Reconcile a proxy death against pending and committed supervision.
    /// Pending reports latch once for the later apply completion; committed
    /// reports evict and record once; repeats are inert.
    pub fn observe_engine_death(&mut self, engine_id: EngineId, reason: DeathReason) -> EngineDeathDisposition {
        if let Some(pending) = self.pending_engines.get_mut(&engine_id) {
            return if pending.early_death.is_none() {
                pending.early_death = Some(reason);
                EngineDeathDisposition::PendingLatched
            } else {
                EngineDeathDisposition::PendingDuplicate
            };
        }
        if let Some(entry) = self.engines.remove(&engine_id) {
            self.record_death(engine_id.0.to_string(), entry.rpc_port, reason);
            return EngineDeathDisposition::LiveRemoved(Box::new(entry.supervision));
        }
        EngineDeathDisposition::Unknown
    }
}

#[runtime]
impl NativeActor for FleetServer {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// supervised-fleet table plus the content-addressed artifact store.
    type State = FleetServerState;
    type Config = FleetConfig;
    const NAMESPACE: &'static str = "aether.fleet";

    fn init(config: FleetConfig, ctx: &mut NativeInitCtx<'_>) -> Result<FleetServerState, BootError> {
        // Build the hub-scoped store from `FleetConfig` (ADR-0090): the
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
            .map_or_else(ArtifactStore::default_root, |dir| PathBuf::from(dir).join(LAYOUT_VERSION_DIR));
        let mut store = ArtifactStore::open(&store_dir, config.binary_disk_budget_bytes)
            .map_err(|e| BootError::Other(Box::new(e)))?;
        bootstrap_ingest(&mut store, &config.binary_bootstrap);
        Ok(FleetServerState {
            engines: HashMap::new(),
            pending_engines: HashMap::new(),
            next_engine_seq: 1,
            mailer: ctx.mailer(),
            heartbeat: config.heartbeat_params(),
            connect_budget: config.connect_budget(),
            spawn_attempts: config.spawn_attempts(),
            fleet_store_root: resolve_fleet_store_root(config.fleet_store_root.as_deref()),
            recently_died: VecDeque::new(),
            store,
            self_mailbox: ctx.self_id(),
            restart_policy: config.restart_policy(),
            pending_restarts: HashMap::new(),
            next_restart_token: 1,
        })
    }

    /// Enumerate every engine the cap currently supervises.
    ///
    /// # Agent
    /// Send `ListEngines` (fieldless). Reply: `ListEnginesResult
    /// { engines: [{ engine_id, rpc_port, last_heartbeat_age_millis }],
    /// recently_died: [{ engine_id, rpc_port, reason, died_age_millis }] }`.
    #[handler::single]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListEngines) -> ListEnginesResult {
        let now = Instant::now();
        let engines = state
            .engines
            .iter()
            .map(|(id, entry)| EngineDescriptor {
                engine_id: id.0.to_string(),
                rpc_port: entry.rpc_port,
                last_heartbeat_age_millis: u64::try_from(now.saturating_duration_since(entry.last_alive).as_millis())
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
                died_age_millis: u64::try_from(now.saturating_duration_since(record.died_at).as_millis())
                    .unwrap_or(u64::MAX),
            })
            .collect();
        ListEnginesResult { engines, recently_died }
    }

    /// Fork+exec a substrate binary and connect a proxy to it.
    ///
    /// # Agent
    /// Send `SpawnEngine { selector, args, boot_manifest }`. The cap
    /// resolves `selector` against its content-addressed binary store
    /// (ADR-0115), materializes the resolved bytes to an executable
    /// temp file, assigns a free localhost port for the substrate's
    /// RPC server, appends it as the `--rpc-port` argv overlay flag
    /// (ADR-0162: config is addressed via argv, never ambient env),
    /// forks the realized binary, then boots an
    /// `aether.fleet.proxy:<id>` actor that dials it. Reply:
    /// `SpawnEngineResult::Ok { engine_id, rpc_port }`
    /// on success, or `Err { engine_id, error }` if the selector
    /// resolves to no stored binary, the fork fails, or the substrate
    /// never comes up. A post-allocation failure carries the allocated
    /// `engine_id` (`Some`) and records a `SpawnFailed` death in the
    /// recently-died ring, so a caller can correlate and reap; a
    /// pre-allocation failure (selector miss, port allocation) carries
    /// `None`. Process preparation remains synchronous, but success is
    /// replied only after the registry owner authoritatively activates the
    /// staged proxy.
    #[handler::manual]
    fn on_spawn(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual, Self>, mail: SpawnEngine) {
        let mut owed: DeferredReply = ctx.defer_reply_to(ctx.reply_target());

        // Resolve the registry selector to stored content bytes before
        // any side effect, so a miss returns without reserving a port
        // or burning an engine id (ADR-0115, #1954).
        let Some(artifact) = resolve_selector(&mut state.store, &mail.selector) else {
            // Pre-allocation failure: no engine id minted yet, so there
            // is nothing to correlate or reap — `engine_id` is `None`.
            owed.reply(
                ctx,
                &SpawnEngineResult::Err {
                    engine_id: None,
                    error: format!("no binary in the registry matched selector {:?}", mail.selector),
                },
            );
            return;
        };

        // Bounded re-fork (issue 2422): a freshly-forked substrate can
        // lose its guessed RPC port to another socket in
        // `free_local_port`'s TOCTOU window and exit on its fatal bind,
        // surfacing as a child-exited-during-startup failure. Each
        // attempt allocates a *fresh* port (and engine id / scratch
        // dir) and re-forks; the theft is per-port and independent
        // across attempts, so N attempts drop the failure probability
        // geometrically. Any non-re-forkable failure returns
        // immediately. The proxy already kills the child it owns on a
        // failed init, so an abandoned attempt leaves no orphan.
        //
        // Each terminal post-allocation failure records a `SpawnFailed`
        // death and carries the minted `engine_id` back (issue 2423) so a
        // caller can correlate and reap; a transient re-forked attempt is
        // recovered, not a real death, so it records nothing.
        let attempts = state.spawn_attempts;
        let recipe = SpawnRecipe {
            hash: artifact.hash.clone(),
            args: mail.args.clone(),
            boot_manifest: mail.boot_manifest.clone(),
        };
        let mut last_error = String::new();

        for attempt in 0..attempts {
            let prepared = match state.prepare_fork(&artifact.path, &recipe) {
                Ok(prepared) => prepared,
                // No engine id was minted, so there is nothing to
                // correlate or reap and no death to record.
                Err(PrepareFailure::PreAllocation(error)) => {
                    owed.reply(ctx, &SpawnEngineResult::Err { engine_id: None, error });
                    return;
                }
                // An id is minted but no engine was ever registered, so
                // record a `SpawnFailed` death and carry the id back so a
                // caller can correlate and reap.
                Err(PrepareFailure::PostAllocation { engine_id, rpc_port, error }) => {
                    owed.reply(ctx, &state.fail_spawn(engine_id, rpc_port, error));
                    return;
                }
            };

            let PreparedFork { engine_id, rpc_port, rpc_addr, child } = prepared;
            let subname = engine_id.0.simple().to_string();

            // `continue_from` still runs `FleetProxy::init` on this thread:
            // it dials the substrate (retrying while it comes up) and, on
            // failure, terminates the child it was handed. A successful
            // init transfers the original caller obligation into the staged
            // birth; only its later task completion may commit the engine.
            let result = ctx
                .spawn_child::<FleetProxy>(
                    Subname::Named(&subname),
                    FleetProxyConfig {
                        engine_id,
                        rpc_addr,
                        spawned: Some(child),
                        heartbeat: state.heartbeat,
                        connect_budget: state.connect_budget,
                    },
                    (),
                )
                .continue_from(
                    owed,
                    FleetSpawnContext {
                        engine_id,
                        rpc_port,
                        supervision: Supervision::new(recipe.clone()),
                        origin: SpawnOrigin::Requested,
                    },
                );

            match result {
                Ok(_) => {
                    let replaced =
                        state.pending_engines.insert(engine_id, PendingEngine { rpc_port, early_death: None });
                    debug_assert!(replaced.is_none(), "fresh engine ids cannot replace a pending spawn");
                    return;
                }
                Err((e, returned)) => {
                    owed = returned;
                    last_error = format!("proxy failed to connect to the spawned substrate: {e:?}");
                    // Re-fork only the bind-stolen-port child-exited
                    // death, and only if attempts remain. Any other
                    // failure is terminal — re-forking it would just
                    // burn the budget again.
                    if is_reforkable_spawn_failure(&e) && attempt + 1 < attempts {
                        tracing::warn!(
                            target: "aether_substrate::fleet_server",
                            engine_id = %engine_id.0,
                            rpc_port,
                            attempt = attempt + 1,
                            attempts,
                            "engine spawn: substrate exited during startup (likely a stolen RPC port); re-forking on a fresh port",
                        );
                        continue;
                    }
                    owed.reply(ctx, &state.fail_spawn(engine_id, rpc_port, last_error));
                    return;
                }
            }
        }

        // Only reached if `attempts` is 0, which `spawn_attempts()`
        // clamps away — keep an honest terminal `Err` rather than an
        // unreachable panic.
        owed.reply(ctx, &SpawnEngineResult::Err { engine_id: None, error: last_error });
    }

    /// Settle one staged proxy birth. Only an authoritative apply with no
    /// earlier proxy death becomes publicly supervised. Owner rejection
    /// arrives after prepared-state rollback has dropped `FleetProxyState`,
    /// which kills and reaps its sole `Child` owner.
    #[handler(task)]
    fn on_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome, FleetSpawnContext>,
    ) {
        let spawn = done.context().clone();
        let engine_id = spawn.engine_id;
        let origin = spawn.origin;
        let outcome = match &done.output().result {
            Ok(()) => ProxySpawnOutcome::Applied(done.output().mailbox_id),
            Err(error) => ProxySpawnOutcome::Rejected(format!("proxy activation failed: {error:?}")),
        };
        let Some(reply) = state.settle_pending_spawn(spawn, outcome) else {
            tracing::warn!(
                target: "aether_substrate::fleet_server",
                engine_id = %engine_id.0,
                "stale proxy spawn completion ignored",
            );
            done.release_no_reply();
            return;
        };

        match origin {
            SpawnOrigin::Requested => done.resolve_value(ctx, &reply),
            // A restart has no caller waiting on a `SpawnEngineResult`,
            // so the settlement hold is released without one. The
            // settle above still ran, so the recovered engine is
            // supervised (or its failure recorded) either way; all that
            // is skipped is the reply nobody asked for.
            SpawnOrigin::Restarted => {
                match &reply {
                    SpawnEngineResult::Ok { .. } => tracing::info!(
                        target: "aether_substrate::fleet_server",
                        engine_id = %engine_id.0,
                        "engine restart: the replacement engine is supervised",
                    ),
                    SpawnEngineResult::Err { error, .. } => tracing::error!(
                        target: "aether_substrate::fleet_server",
                        engine_id = %engine_id.0,
                        error = %error,
                        "engine restart: the replacement engine did not commit; the engine stays dead",
                    ),
                }
                done.release_no_reply();
            }
        }
    }

    /// Terminate a supervised engine.
    ///
    /// # Agent
    /// Send `TerminateEngine { engine_id }` (the string from a
    /// prior `SpawnEngineResult` / `ListEnginesResult`). The cap
    /// forwards the kind to the engine's proxy — which terminates
    /// its substrate's process group and self-shuts-down — and drops its table
    /// entry. Reply: `TerminateEngineResult::Ok`, or `Err { error }`
    /// for an `engine_id` that doesn't parse or names no
    /// supervised engine.
    #[handler::single]
    fn on_terminate(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: TerminateEngine) -> TerminateEngineResult {
        let engine_id = match Uuid::parse_str(&mail.engine_id) {
            Ok(uuid) => EngineId(uuid),
            Err(e) => {
                return TerminateEngineResult::Err {
                    error: format!("engine_id {:?} is not a valid UUID: {e}", mail.engine_id),
                };
            }
        };

        let Some(entry) = state.engines.remove(&engine_id) else {
            return TerminateEngineResult::Err { error: format!("no supervised engine {}", mail.engine_id) };
        };

        // Record the deliberate shutdown in the recently-died ring so
        // `list_engines` can show it left cleanly (issue 1906). The
        // proxy deliberately does not `report_died` for a terminate —
        // the cap initiated it — so there is no second signal to
        // reconcile and this is the one record for this death.
        let proxy_mailbox = entry.proxy_mailbox;
        state.record_death(mail.engine_id.clone(), entry.rpc_port, DeathReason::Terminated);

        // Forward to the proxy: it terminates its substrate's group and
        // self-shuts-down. Fire-and-forget — the proxy doesn't
        // reply, and the table entry is already gone, so the
        // returned MailId has nothing to subscribe against.
        let payload = mail.encode_into_bytes();
        let _ = ctx.send_envelope_tracked(proxy_mailbox, <TerminateEngine as Kind>::ID, &payload);
        TerminateEngineResult::Ok
    }

    /// Relay one mail to a specific engine's substrate.
    ///
    /// # Agent
    /// Not a user-facing tool — the hub's `RpcServerCapability`
    /// sends this when an RPC client addresses a `Call` at
    /// `engine = Some(_)`. The cap looks the engine up in its
    /// table and re-emits a `ForwardEnvelope` at the matching
    /// `aether.fleet.proxy:<id>`, propagating the inbound
    /// reply-to verbatim so the substrate's reply (and the proxy's
    /// terminal `CallSettled`) stream straight back to that
    /// `RpcServerCapability`. An unknown / unparseable `engine_id`
    /// is answered with `CallSettled::Err` so the originating wire
    /// call closes instead of hanging.
    #[handler::single]
    fn on_route(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: RouteEnvelope) {
        let reply_to = ctx.reply_target();
        let SourceAddr::Component(reply_target) = reply_to.addr else {
            // A routed call always carries a Component reply-to
            // (the originating RpcServerCapability). Without one
            // there's nowhere to stream the reply or the
            // CallSettled — drop rather than guess.
            tracing::warn!(
                target: "aether_substrate::fleet_server",
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
            settle_err(&state.mailer, reply_target, correlation, format!("no supervised engine {}", mail.engine_id));
            return;
        };

        // Re-emit as a ForwardEnvelope at the proxy, carrying the
        // inbound reply-to verbatim so the substrate's reply — and
        // the proxy's CallSettled — route straight back to the
        // originating RpcServerCapability.
        let forward = ForwardEnvelope { mailbox: mail.mailbox, kind: mail.kind, payload: mail.payload };
        state.mailer.push(
            Mail::new(entry.proxy_mailbox, <ForwardEnvelope as Kind>::ID, forward.encode_into_bytes(), 1)
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
    #[handler::single]
    fn on_engine_died(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: EngineDied) {
        let Ok(uuid) = Uuid::parse_str(&mail.engine_id) else {
            tracing::warn!(
                target: "aether_substrate::fleet_server",
                engine_id = %mail.engine_id,
                "engine died: unparseable engine_id; ignoring",
            );
            return;
        };
        match state.observe_engine_death(EngineId(uuid), mail.reason.clone()) {
            EngineDeathDisposition::PendingLatched => {
                tracing::info!(
                    target: "aether_substrate::fleet_server",
                    engine_id = %mail.engine_id,
                    reason = ?mail.reason,
                    "engine death latched while proxy activation is pending",
                );
            }
            EngineDeathDisposition::LiveRemoved(supervision) => {
                tracing::info!(
                    target: "aether_substrate::fleet_server",
                    engine_id = %mail.engine_id,
                    reason = ?mail.reason,
                    "engine evicted: proxy reported death",
                );
                state.consider_restart(&mail.engine_id, &mail.reason, *supervision);
            }
            EngineDeathDisposition::PendingDuplicate | EngineDeathDisposition::Unknown => {}
        }
    }

    /// Re-fork an engine whose restart backoff has elapsed.
    ///
    /// # Agent
    /// Not a user-facing tool — the cap's own restart-backoff timer
    /// fires this at itself after `restart_backoff_millis`. The handler
    /// looks the token up and re-forks the filed recipe under a fresh
    /// engine id. A token with no pending entry is a silent no-op.
    #[handler::single]
    fn on_restart_due(state: &mut Self::State, ctx: &mut NativeCtx<'_, Single, Self>, mail: EngineRestartDue) {
        if let Some(supervision) = state.pending_restarts.remove(&mail.token) {
            state.restart_engine(ctx, supervision);
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
    #[handler::single]
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
    /// `Err { error }` for an unreadable path, a `--describe` that
    /// failed or didn't yield a parseable manifest, or a store write
    /// that didn't land — an `Ok` hash is always resolvable.
    #[handler::single]
    fn on_upload_binary(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UploadBinary) -> UploadBinaryResult {
        match ingest_binary(&mut state.store, &mail.staged_path, mail.name.clone()) {
            Ok(hash) => UploadBinaryResult::Ok { hash, name: mail.name },
            Err(error) => UploadBinaryResult::Err { error },
        }
    }

    /// Enumerate the hub's stored engine binaries.
    ///
    /// # Agent
    /// Send `ListEngineBinaries { chassis?, caps, target?, limit?,
    /// include_history }` (attribute filters AND-combined; an absent / empty
    /// field is no constraint). Reply: `ListEngineBinariesResult { binaries,
    /// total_matched }`, with a stable newest-first page produced before
    /// entries cross the wire.
    #[handler::single]
    fn on_list_engine_binaries(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ListEngineBinaries,
    ) -> ListEngineBinariesResult {
        state.store.list_binaries_page(&mail)
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
    /// `Err { error }` for an unreadable path, an unparseable wasm, or a
    /// store write that didn't land — an `Ok` hash is always resolvable.
    #[handler::single]
    fn on_upload_component(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: UploadComponent,
    ) -> UploadComponentResult {
        match ingest_component(&mut state.store, &mail.staged_path, mail.name.clone()) {
            Ok(hash) => UploadComponentResult::Ok { hash, name: mail.name },
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
    #[handler::single]
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
    /// Send `ListComponentBinaries { namespace?, handled_kind?, limit?,
    /// include_history }` (attribute filters AND-combined; an absent field is
    /// no constraint). Reply: `ListComponentBinariesResult { components,
    /// total_matched }`, with a stable newest-first page produced before
    /// entries cross the wire.
    #[handler::single]
    fn on_list_component_binaries(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ListComponentBinaries,
    ) -> ListComponentBinariesResult {
        state.store.list_components_page(&mail)
    }
}
