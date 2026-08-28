//! `aether.fleet` — engines capability (issue 763 P4).
//!
//! A singleton `NativeActor` that supervises a fleet of
//! `FleetProxy` actors — the engine-management surface of the
//! forward-model architecture (issue 763). Three handlers:
//!
//! - **`on_spawn`** ([`SpawnEngine`]) picks a free localhost port,
//!   fork+execs the substrate binary with the port addressed as
//!   `--rpc-port` argv (ADR-0162; the child's environment is constructed
//!   from an allowlist at fork, never inherited, so no `AETHER_*` key
//!   crosses), then boots an `aether.fleet.proxy:<id>` child actor that dials
//!   it. The proxy owns the forked child from there — startup-dial
//!   retry, kill-on-failed-boot, kill-on-drop. Reply:
//!   `SpawnEngineResult`.
//! - **`on_list`** ([`ListEngines`]) reports every supervised engine.
//! - **`on_terminate`** ([`TerminateEngine`]) forwards the kind to the
//!   engine's proxy (which terminates its substrate's process group
//!   and self-shuts-down)
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
//! `std::process::Child` handle into the proxy, so its substrate-typed
//! runtime half lives in the `runtime` module. The `#[actor]` macro divides
//! the identity from that runtime (ADR-0122): the [`FleetServer`] ZST and
//! its addressing markers stay always-on, while the state and handlers live
//! behind the `runtime` seam.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root — the
// `#[actor]` macro emits `impl HandlesKind<K>` markers always-on against
// the identity, so they reference these kinds from here. The per-handler
// reply kinds those markers also name arrive through the `use runtime::*`
// glob below.
use crate::kinds::{EngineAlive, EngineDied, EngineRestartDue};
use aether_kinds::{
    ListComponentBinaries, ListEngineBinaries, ListEngines, ResolveComponent, SpawnEngine, TerminateEngine,
    UploadBinary, UploadComponent,
};
use aether_rpc::RouteEnvelope;
#[cfg(test)]
use std::sync::{Arc, Mutex};

// The engines cap's implementation, split along its seams (ADR-0121):
// `config` (the ADR-0090 config struct + parsers), `artifacts` (the
// content-addressed store resolution / ingestion the handlers delegate
// to), and `fleet` (free-port allocation, routed-call settlement, and
// spawn-dir resolution). All three are native-only — the cap forks
// processes and owns sockets — so they elide on wasm alongside the
// runtime half.
#[cfg(not(target_family = "wasm"))]
mod artifacts;
#[cfg(not(target_family = "wasm"))]
mod config;
#[cfg(not(target_family = "wasm"))]
mod fleet;
// The restart-backoff timer sidecar (one-shot thread + wake-mail), kept
// beside the other native-only halves for the same reason: it owns an OS
// thread.
#[cfg(not(target_family = "wasm"))]
mod restart;

// `FleetConfig` (+ its derive-emitted `FleetOverlay`) ride through
// file root for the hub chassis bin, which flattens the overlay into
// `HubCli`, resolves argv-then-env, and passes the config to
// `with_actor::<FleetServer>(cfg)` (ADR-0090). Native-only re-export —
// the engines cap is native-only, so the config has no wasm consumer.
#[cfg(not(target_family = "wasm"))]
pub use config::{FleetConfig, FleetConfigLayer, FleetOverlay, RestartPolicy};

/// `aether.fleet` engines-cap **identity** (ADR-0122 identity/runtime
/// split). A ZST carrying only the addressing — `Addressable` (`NAMESPACE`,
/// `Resolver`), the per-handler `HandlesKind` markers, and the
/// name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`runtime::FleetServerState`, which holds the
/// supervised-fleet table + the `aether_substrate`-typed mailer + the
/// artifact store) lives in `runtime.rs`, so the identity file never names
/// `FleetServerState`.
#[actor(singleton, root)]
pub struct FleetServer;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state, the artifact/fleet helpers — lives
// in the `runtime` module below; the `#[actor] impl` reaches all of it through
// the single `use runtime::*` glob.
// The handler-signature kinds (`ListEngines` / `SpawnEngine` / …) stay
// always-on at file root — the always-on `HandlesKind<K>` markers name them.
use aether_actor::actor;

// The `runtime` module is this cap's private runtime-half namespace; the impl
// reaches all of it (state, ctx types, artifact/fleet helpers, result kinds)
// through this single seam, so the glob is intentional rather than a few dozen
// one-line imports.
#[allow(clippy::wildcard_imports)]
use runtime::*;

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `FleetServerState`, the `EngineEntry` / `DeadRecord` helper types, the
// `record_death` helper) — lives in `runtime.rs`. The `#[actor] impl` above
// reaches it through the `use runtime::*` glob.
mod runtime;

// The `#[cfg(test)]` [`ReplySink`] is a field-bearing test fixture, so it
// stays the un-split `type State = Self` shape (ADR-0122). Its substrate-typed
// surface (`NativeActor` / `NativeCtx` / `NativeInitCtx` / `BootError`) and its
// reply-kind handler signatures (`ListEnginesResult` / … — named by the
// always-on `HandlesKind<K>` markers) resolve through the same
// `use runtime::*` glob the `FleetServer` impl uses.

/// Reply sink: records the latest reply of each engines-cap reply
/// kind into shared cells so a unit test can drive a handler via
/// `mailer.push` and observe what it replied. Lives at file root (not
/// nested in `mod tests`) so the `#[actor]` macro's marker emission
/// stays addressable.
// `pub` rather than private because it's the `NativeActor::Config` of
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
pub struct ReplySink {
    cells: ReplyCells,
}

#[cfg(test)]
#[actor(singleton, root)]
impl NativeActor for ReplySink {
    // ADR-0156 §3: the shared capture cells are construction wiring, not
    // operator config, so they ride the `Params` channel; `Config` is `()`.
    type Config = ();
    type Params = ReplyCells;
    const NAMESPACE: &'static str = "aether.fleet.test.reply_sink";

    fn init((): (), cells: ReplyCells, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { cells })
    }

    #[handler::single]
    fn on_list_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: ListEnginesResult) {
        *self.cells.list.lock().expect("test setup: list cell mutex poisoned") = Some(reply);
    }

    #[handler::single]
    fn on_spawn_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: SpawnEngineResult) {
        *self.cells.spawn.lock().expect("test setup: spawn cell mutex poisoned") = Some(reply);
    }

    #[handler::single]
    fn on_terminate_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: TerminateEngineResult) {
        *self.cells.terminate.lock().expect("test setup: terminate cell mutex poisoned") = Some(reply);
    }
}

#[cfg(test)]
mod tests {
    // Test harness resolves the server/sink actor mailboxes by their NAMESPACE
    // for fixture wiring — reference id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use super::runtime::{
        EngineEntry, FleetServerState, FleetSpawnContext, PendingEngine, ProxySpawnOutcome, SpawnOrigin, SpawnRecipe,
        Supervision, spawn_args,
    };
    use super::{FleetConfig, FleetServer, ReplyCells, ReplySink, RestartPolicy};
    use crate::kinds::{EngineAlive, EngineDied};
    use crate::store::{ArtifactStore, DEFAULT_DISK_BUDGET_BYTES};
    use aether_actor::Addressable;
    use aether_data::{EngineId, Kind, MailboxId, Uuid, mailbox_id_from_name};
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
    use aether_substrate::testing::{TestChassis, boot_authority};
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use std::{env, fs, process, thread};

    /// Boot a passive chassis hosting `FleetServer` + the reply sink.
    /// Returns the chassis (kept alive for its dispatcher threads), the
    /// mailer to push requests through, and the sink's cells.
    fn boot() -> (PassiveChassis<TestChassis>, Arc<Mailer>, ReplyCells) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(&boot_authority(), d);
        }
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
        let cells = ReplyCells::default();
        // Point the cap's binary store (ADR-0115) at a per-call temp dir via
        // the ADR-0090 config field so these unit tests never touch the real
        // `dirs::data_dir()` store. Heartbeat stays disabled (the `Default`);
        // only the store dir is overridden.
        let config = FleetConfig { binary_store_dir: Some(isolated_store_dir()), ..FleetConfig::default() };
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor_configured::<FleetServer>((), config)
            .with_actor::<ReplySink>(cells.clone())
            .build_passive()
            .expect("caps boot");
        (chassis, mailer, cells)
    }

    /// A unique per-call temp dir for the engines-cap unit tests' binary
    /// store (ADR-0115), threaded onto `FleetConfig`'s `binary_store_dir`
    /// by [`boot`] so they never touch the real `dirs::data_dir()` store. No
    /// env side-channel — the store dir now rides the config (ADR-0090).
    fn isolated_store_dir() -> String {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        env::temp_dir().join(format!("aether-binstore-engcap-{}-{nanos}", process::id())).to_string_lossy().into_owned()
    }

    /// Minimal state for deterministic lifecycle reducer tests. These tests
    /// manually step pending/death/apply order; the integration suite owns
    /// scheduler interaction and real process teardown.
    ///
    /// `restart_policy` selects whether restart supervision is armed, so
    /// the same fixture serves both the historical death-is-terminal
    /// reducers and the restart ones.
    fn lifecycle_state(restart_policy: Option<RestartPolicy>) -> (FleetServerState, PathBuf) {
        let root = PathBuf::from(isolated_store_dir());
        let store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES).expect("test lifecycle store opens");
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        (
            FleetServerState {
                engines: HashMap::new(),
                pending_engines: HashMap::new(),
                next_engine_seq: 1,
                mailer,
                heartbeat: None,
                connect_budget: None,
                spawn_attempts: 1,
                fleet_store_root: root.join("engines"),
                recently_died: VecDeque::new(),
                store,
                self_mailbox: MailboxId(0x_FEE7_0000),
                restart_policy,
                pending_restarts: HashMap::new(),
                next_restart_token: 1,
            },
            root,
        )
    }

    /// A recipe standing in for one a real spawn would have retained.
    /// The hash is deliberately not in the fixture store — the reducer
    /// tests here stop at the restart *decision*; re-resolving the hash
    /// and forking belong to the integration suite.
    fn test_recipe() -> SpawnRecipe {
        SpawnRecipe { hash: "0".repeat(64), args: vec!["--seed".into(), "7".into()], boot_manifest: None }
    }

    /// Drive one request kind at `aether.fleet`, reply-to the sink,
    /// and block until `probe` sees a recorded reply (or the deadline
    /// passes).
    fn drive<K: Kind, T>(mailer: &Arc<Mailer>, request: &K, probe: impl Fn() -> Option<T>) -> T {
        let server = mailbox_id_from_name(<FleetServer as Addressable>::NAMESPACE);
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
    fn push_then_list<K: Kind>(mailer: &Arc<Mailer>, cells: &ReplyCells, fire: &K) -> aether_kinds::ListEnginesResult {
        let server = mailbox_id_from_name(<FleetServer as Addressable>::NAMESPACE);
        mailer.push(Mail::new(server, K::ID, fire.encode_into_bytes(), 1));
        drive(mailer, &ListEngines {}, || cells.list.lock().expect("test setup: list cell mutex poisoned").take())
    }

    /// `on_list` on a fresh cap replies with an empty engine list.
    #[test]
    fn list_on_empty_cap_is_empty() {
        let (_chassis, mailer, cells) = boot();
        let result =
            drive(&mailer, &ListEngines {}, || cells.list.lock().expect("test setup: list cell mutex poisoned").take());
        assert!(result.engines.is_empty(), "fresh cap supervises no engines");
    }

    /// The fleet table changes only at authoritative completion. This is a
    /// controlled reducer proof, not a scheduler-order proof: the real-process
    /// integration test below covers the owner/task wake path.
    #[test]
    fn pending_proxy_is_invisible_until_authoritative_apply() {
        let (mut state, root) = lifecycle_state(None);
        let engine_id = EngineId(Uuid::from_u128(1));
        let rpc_port = 40_680;
        state.pending_engines.insert(engine_id, PendingEngine { rpc_port, early_death: None });

        assert!(state.engines.is_empty(), "a prepared proxy is not yet supervised");
        let reply = state
            .settle_pending_spawn(
                FleetSpawnContext {
                    engine_id,
                    rpc_port,
                    supervision: Supervision::new(test_recipe()),
                    origin: SpawnOrigin::Requested,
                },
                ProxySpawnOutcome::Applied(MailboxId(0x4068)),
            )
            .expect("the matching completion settles once");

        assert!(matches!(reply, SpawnEngineResult::Ok { rpc_port: port, .. } if port == rpc_port));
        assert!(state.pending_engines.is_empty(), "completion consumes the reservation");
        assert_eq!(state.engines.get(&engine_id).map(|entry| entry.proxy_mailbox), Some(MailboxId(0x4068)));

        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    /// A proxy can report death from its catch-up wake before the parent task
    /// turn. Manual stepping proves the interleaving: the first report wins,
    /// apply cannot install a corpse, and stale/duplicate settlement creates
    /// neither a second reply value nor a second death record.
    #[test]
    fn early_proxy_death_wins_over_apply_once() {
        let (mut state, root) = lifecycle_state(None);
        let engine_id = EngineId(Uuid::from_u128(2));
        let rpc_port = 40_681;
        let spawn = FleetSpawnContext {
            engine_id,
            rpc_port,
            supervision: Supervision::new(test_recipe()),
            origin: SpawnOrigin::Requested,
        };
        state.pending_engines.insert(engine_id, PendingEngine { rpc_port, early_death: None });

        state.observe_engine_death(
            engine_id,
            DeathReason::Crashed { detail: "connection closed during activation".to_owned() },
        );
        state.observe_engine_death(
            engine_id,
            DeathReason::Evicted { detail: "duplicate late heartbeat report".to_owned() },
        );

        let reply = state
            .settle_pending_spawn(spawn.clone(), ProxySpawnOutcome::Applied(MailboxId(0x4068_0002)))
            .expect("the first completion settles");
        assert!(
            matches!(reply, SpawnEngineResult::Err { engine_id: Some(ref id), ref error }
                if id == &engine_id.0.to_string() && error.contains("died before supervision committed")),
            "an early-dead proxy returns an id-bearing failure: {reply:?}",
        );
        assert!(state.engines.is_empty(), "authoritative apply cannot install the reported-dead proxy");
        assert_eq!(state.recently_died.len(), 1, "the first death is recorded exactly once");
        assert!(matches!(state.recently_died[0].reason, DeathReason::Crashed { ref detail }
            if detail == "connection closed during activation"));

        assert!(
            state.settle_pending_spawn(spawn, ProxySpawnOutcome::Rejected("stale owner result".to_owned())).is_none(),
            "a duplicate/stale completion has no second reply value",
        );
        assert_eq!(state.recently_died.len(), 1, "a stale completion cannot duplicate the death record");

        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    /// Owner rejection is distinct from an init/refork failure: it consumes
    /// the pending record, never exposes a live row, and records one
    /// id-bearing `SpawnFailed` result for the caller.
    #[test]
    fn owner_rejection_settles_pending_spawn_as_failed_once() {
        let (mut state, root) = lifecycle_state(None);
        let engine_id = EngineId(Uuid::from_u128(3));
        let rpc_port = 40_682;
        state.pending_engines.insert(engine_id, PendingEngine { rpc_port, early_death: None });

        let reply = state
            .settle_pending_spawn(
                FleetSpawnContext {
                    engine_id,
                    rpc_port,
                    supervision: Supervision::new(test_recipe()),
                    origin: SpawnOrigin::Requested,
                },
                ProxySpawnOutcome::Rejected("canonical route collision".to_owned()),
            )
            .expect("matching owner rejection settles");
        assert!(matches!(reply, SpawnEngineResult::Err { engine_id: Some(ref id), .. }
            if id == &engine_id.0.to_string()));
        assert!(state.engines.is_empty());
        assert!(state.pending_engines.is_empty());
        assert_eq!(state.recently_died.len(), 1);
        assert!(matches!(state.recently_died[0].reason, DeathReason::SpawnFailed { .. }));

        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    /// A policy with a wide-open window, so a test that never sleeps can
    /// exercise the burst limit without the window aging entries out
    /// underneath it.
    fn restart_policy(burst_limit: u32) -> RestartPolicy {
        RestartPolicy { backoff: Duration::from_millis(1), burst_limit, burst_window: Duration::from_hours(1) }
    }

    /// Install one supervised engine so a death has something to evict.
    fn supervise(state: &mut FleetServerState, engine_id: EngineId) {
        state.engines.insert(
            engine_id,
            EngineEntry {
                proxy_mailbox: MailboxId(0x4068),
                rpc_port: 7000,
                last_alive: Instant::now(),
                supervision: Supervision::new(test_recipe()),
            },
        );
    }

    /// A deliberate `TerminateEngine` must stay dead even with restart
    /// supervision armed. The operator asked for that engine to be gone;
    /// a cap that forked it straight back would make `terminate_substrate`
    /// unable to do the one thing it exists for. `SpawnFailed` is
    /// excluded for a different reason — no supervised engine ever
    /// existed to recover — and `on_spawn`'s own bounded re-fork owns
    /// that retry, so a restart here would double it.
    #[test]
    fn restart_supervision_acts_on_crash_and_eviction_only() {
        let crashed = DeathReason::Crashed { detail: "connection closed".to_owned() };
        let evicted = DeathReason::Evicted { detail: "heartbeat miss limit 3 of 3".to_owned() };
        let spawn_failed = DeathReason::SpawnFailed { detail: "never connected".to_owned() };

        assert!(super::runtime::restart_applies_to(&crashed), "a crash is what restart supervision is for");
        assert!(super::runtime::restart_applies_to(&evicted), "a wedged engine evicted on heartbeat is recoverable");
        assert!(!super::runtime::restart_applies_to(&DeathReason::Terminated), "a deliberate terminate stays dead");
        assert!(!super::runtime::restart_applies_to(&spawn_failed), "a failed spawn has no engine to recover");
    }

    /// The decision itself, not just the predicate feeding it: under an
    /// armed policy a crash schedules a restart and a deliberate
    /// terminate does not. This is what fails if the reason check is ever
    /// dropped from `consider_restart` while `restart_applies_to` keeps
    /// passing its own unit test — and the positive half keeps the
    /// negative half from passing because nothing is wired at all.
    #[test]
    fn an_armed_policy_schedules_on_a_crash_and_refuses_a_terminate() {
        let (mut state, root) = lifecycle_state(Some(restart_policy(5)));
        let engine_id = EngineId(Uuid::from_u128(0xA1)).0.to_string();

        let crashed = DeathReason::Crashed { detail: "connection closed".to_owned() };
        assert!(
            state.consider_restart(&engine_id, &crashed, Supervision::new(test_recipe())),
            "a crash under an armed policy is restarted",
        );
        assert_eq!(state.pending_restarts.len(), 1, "the crash files exactly one pending restart");

        assert!(
            !state.consider_restart(&engine_id, &DeathReason::Terminated, Supervision::new(test_recipe())),
            "a deliberate terminate is never restarted",
        );
        assert_eq!(state.pending_restarts.len(), 1, "the terminate filed nothing of its own");

        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    /// With supervision off — the default every existing harness reads —
    /// a crash schedules nothing, and the eviction-and-record path is
    /// untouched. Pins the opt-in: an accidental default-on would change
    /// what the whole existing suite observes.
    #[test]
    fn a_crash_without_a_policy_stays_terminal() {
        let (mut state, root) = lifecycle_state(None);
        let engine_id = EngineId(Uuid::from_u128(0xA2));
        supervise(&mut state, engine_id);

        let disposition = state.observe_engine_death(engine_id, DeathReason::Crashed { detail: "bye".to_owned() });
        let super::runtime::EngineDeathDisposition::LiveRemoved(supervision) = disposition else {
            panic!("a supervised engine's death evicts it");
        };
        assert!(state.engines.is_empty(), "the corpse is still evicted");
        assert_eq!(state.recently_died.len(), 1, "the death is still recorded");

        let crashed = DeathReason::Crashed { detail: "bye".to_owned() };
        assert!(!state.consider_restart(&engine_id.0.to_string(), &crashed, *supervision), "no policy, no restart");
        assert!(state.pending_restarts.is_empty(), "restart supervision is off, so nothing is scheduled");

        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    /// The burst limit binds through the real decision path, not only
    /// through `admit_restart` in isolation: once the budget is spent,
    /// `consider_restart` stops filing restarts and the engine stays
    /// dead. Carries one ledger across the calls the way a lineage does.
    #[test]
    fn consider_restart_stops_filing_once_the_budget_is_spent() {
        let policy = restart_policy(2);
        let (mut state, root) = lifecycle_state(Some(policy));
        let engine_id = EngineId(Uuid::from_u128(0xA4)).0.to_string();
        let crashed = DeathReason::Crashed { detail: "boom".to_owned() };
        let mut supervision = Supervision::new(test_recipe());

        for restart in 0..policy.burst_limit {
            assert!(
                state.consider_restart(&engine_id, &crashed, supervision.clone()),
                "restart {restart} is inside the budget",
            );
            // Spend the same instant on the carried ledger, standing in
            // for the successor engine inheriting it through its spawn
            // context.
            supervision.admit_restart(policy, Instant::now());
        }
        assert_eq!(state.pending_restarts.len(), policy.burst_limit as usize);

        assert!(
            !state.consider_restart(&engine_id, &crashed, supervision),
            "past the burst limit the cap gives up rather than restarting",
        );
        assert_eq!(state.pending_restarts.len(), policy.burst_limit as usize, "the refused restart filed nothing");

        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    /// The burst limit binds across a *lineage*, not one engine id. A
    /// restarted engine gets a fresh id, so a budget keyed on engine id
    /// would reset on every restart and never stop a crash loop; the
    /// ledger has to ride the successor's spawn context. Spend the budget
    /// exactly, then assert the next admission is refused — and that the
    /// refusal did not itself charge the ledger, so a caller that keeps
    /// asking cannot push the window forward indefinitely.
    #[test]
    fn the_restart_burst_limit_binds_and_a_refusal_does_not_charge_it() {
        let policy = restart_policy(3);
        let mut supervision = Supervision::new(test_recipe());
        let now = Instant::now();

        for spent in 0..policy.burst_limit {
            assert!(supervision.admit_restart(policy, now), "restart {spent} is inside the budget");
        }
        assert!(!supervision.admit_restart(policy, now), "the restart past the limit is refused");
        assert_eq!(
            supervision.restarts.len(),
            policy.burst_limit as usize,
            "a refused restart must not charge the budget",
        );
    }

    /// Restarts age out of the rolling window, so an engine that crashes
    /// rarely is recovered every time while one crash-looping exhausts
    /// its budget. Steps the clock rather than sleeping: `admit_restart`
    /// takes `now` precisely so the window is testable without wall time.
    #[test]
    fn restarts_older_than_the_window_stop_counting() {
        let policy = restart_policy(2);
        let mut supervision = Supervision::new(test_recipe());
        let start = Instant::now();

        assert!(supervision.admit_restart(policy, start));
        assert!(supervision.admit_restart(policy, start));
        assert!(!supervision.admit_restart(policy, start), "the budget is spent inside the window");

        let past_window = start + policy.burst_window + Duration::from_millis(1);
        assert!(supervision.admit_restart(policy, past_window), "a fully aged-out window restores the budget");
        assert_eq!(supervision.restarts.len(), 1, "the aged-out entries are pruned, not merely ignored");
    }

    /// Tripwire: a restart re-forks from the retained recipe, and this is
    /// the single place a recipe becomes a command line. The order is the
    /// contract — the caller's own args first, then the hub's injected
    /// overlay flags — because a substrate parses argv positionally
    /// against its derive-emitted overlay. If a restart ever loses the
    /// caller's args or its boot manifest, or emits them after the hub's
    /// flags, the recovered engine boots differently from the one it
    /// replaced, silently. Both paths go through `spawn_args`, so pinning
    /// it here pins both.
    #[test]
    fn a_recipe_renders_the_callers_args_ahead_of_the_hubs_injections() {
        let bare = SpawnRecipe { hash: "h".to_owned(), args: vec!["--seed".into(), "7".into()], boot_manifest: None };
        assert_eq!(spawn_args(&bare, 8901), vec!["--seed", "7", "--rpc-port", "8901"]);

        let with_manifest = SpawnRecipe { boot_manifest: Some("/boot.json".to_owned()), ..bare };
        assert_eq!(
            spawn_args(&with_manifest, 8901),
            vec!["--seed", "7", "--rpc-port", "8901", "--boot-manifest", "/boot.json"],
        );
    }

    /// The recipe a spawn retains is what a restart replays, so it must
    /// survive the pending→committed handoff intact. Catches a
    /// `settle_pending_spawn` that installs an entry while dropping or
    /// rebuilding the supervision it was handed — the drift would only
    /// surface much later, as a restart onto the wrong argv.
    #[test]
    fn a_committed_engine_retains_the_recipe_its_spawn_carried() {
        let (mut state, root) = lifecycle_state(None);
        let engine_id = EngineId(Uuid::from_u128(0xA3));
        state.pending_engines.insert(engine_id, PendingEngine { rpc_port: 7100, early_death: None });

        state
            .settle_pending_spawn(
                FleetSpawnContext {
                    engine_id,
                    rpc_port: 7100,
                    supervision: Supervision::new(test_recipe()),
                    origin: SpawnOrigin::Requested,
                },
                ProxySpawnOutcome::Applied(MailboxId(0x4068)),
            )
            .expect("the matching completion settles");

        let entry = state.engines.get(&engine_id).expect("an applied spawn is supervised");
        assert_eq!(entry.supervision.recipe, test_recipe(), "the spawn's recipe rides onto the committed engine");

        drop(state);
        let _ = fs::remove_dir_all(root);
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
            || cells.spawn.lock().expect("test setup: spawn cell mutex poisoned").take(),
        );
        match result {
            SpawnEngineResult::Err { error, .. } => {
                assert!(error.contains("no binary in the registry matched selector"), "unexpected error: {error}");
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
        use std::os::unix::fs::PermissionsExt;

        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-binstore-bootstrap-{}-{nanos}", process::id()));
        fs::create_dir_all(&dir).expect("test setup: bootstrap temp dir");

        // A stand-in chassis bin: on `--describe` it prints a conforming
        // headless manifest (non-empty caps + config surface, so the
        // upload gate accepts it, #3936); its own bytes are what the
        // store content-addresses.
        let stand_in = dir.join("aether-headless");
        fs::write(
            &stand_in,
            "#!/bin/sh\nif [ \"$1\" = \"--describe\" ]; then printf \
                 '{\"chassis\":\"headless\",\"caps\":[\"aether.fs\",\"aether.rpc.server\"],\
                 \"git_sha\":\"deadbee\",\"profile\":\"debug\",\
                 \"target\":\"x86_64-unknown-linux-gnu\",\
                 \"env_keys\":[\"AETHER_RPC_PORT\"],\"argv_flags\":[\"rpc-port\"]}'; fi\n",
        )
        .expect("test setup: write stand-in");
        fs::set_permissions(&stand_in, fs::Permissions::from_mode(0o755)).expect("test setup: chmod stand-in");

        let mut store =
            ArtifactStore::open(&dir.join("store"), DEFAULT_DISK_BUDGET_BYTES).expect("test setup: open store");
        let bootstrap = HashSet::from([stand_in.to_string_lossy().into_owned()]);
        bootstrap_ingest(&mut store, &bootstrap);

        let resolved =
            resolve_selector(&mut store, &BinarySelector { query: None, chassis: None, caps: vec![], target: None })
                .expect("the default selector resolves to the bootstrapped headless bin");
        assert_eq!(
            resolved.manifest.as_binary().expect("the resolved artifact is a binary").chassis,
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

        let malformed = drive(&mailer, &TerminateEngine { engine_id: "not-a-uuid".to_owned() }, || {
            cells.terminate.lock().expect("test setup: terminate cell mutex poisoned").take()
        });
        assert!(matches!(malformed, TerminateEngineResult::Err { .. }), "a malformed engine_id should be rejected");

        let unknown =
            drive(&mailer, &TerminateEngine { engine_id: "00000000-0000-0000-0000-000000000000".to_owned() }, || {
                cells.terminate.lock().expect("test setup: terminate cell mutex poisoned").take()
            });
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
                reason: DeathReason::Crashed { detail: "peer closed".to_owned() },
            },
        );
        assert!(after_malformed.engines.is_empty(), "a malformed died must not panic or insert");
        assert!(after_malformed.recently_died.is_empty(), "a malformed died records no phantom death");

        let after_unknown = push_then_list(
            &mailer,
            &cells,
            &EngineDied {
                engine_id: "00000000-0000-0000-0000-000000000000".to_owned(),
                reason: DeathReason::Evicted { detail: "heartbeat miss limit 3 of 3".to_owned() },
            },
        );
        assert!(after_unknown.engines.is_empty(), "a died for an unknown engine is a no-op");
        assert!(after_unknown.recently_died.is_empty(), "a died for an unknown engine records no phantom death");
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
            &EngineAlive { engine_id: "00000000-0000-0000-0000-000000000000".to_owned() },
        );
        assert!(after.engines.is_empty(), "an alive for an unknown engine must not insert it");
    }
}
