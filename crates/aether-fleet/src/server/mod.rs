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
use crate::kinds::{EngineAlive, EngineDied};
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

// `FleetConfig` (+ its derive-emitted `FleetOverlay`) ride through
// file root for the hub chassis bin, which flattens the overlay into
// `HubCli`, resolves argv-then-env, and passes the config to
// `with_actor::<FleetServer>(cfg)` (ADR-0090). Native-only re-export —
// the engines cap is native-only, so the config has no wasm consumer.
#[cfg(not(target_family = "wasm"))]
pub use config::{FleetConfig, FleetConfigLayer, FleetOverlay};

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
    use super::runtime::{FleetServerState, FleetSpawnContinuation, PendingEngine, ProxySpawnOutcome};
    use super::{FleetConfig, FleetServer, ReplyCells, ReplySink};
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
    use aether_substrate::testing::TestChassis;
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
            let _ = registry.register_kind_with_descriptor(d);
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
    fn lifecycle_state() -> (FleetServerState, PathBuf) {
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
            },
            root,
        )
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
        let (mut state, root) = lifecycle_state();
        let engine_id = EngineId(Uuid::from_u128(1));
        let rpc_port = 40_680;
        state.pending_engines.insert(engine_id, PendingEngine { rpc_port, early_death: None });

        assert!(state.engines.is_empty(), "a prepared proxy is not yet supervised");
        let reply = state
            .settle_pending_spawn(
                FleetSpawnContinuation { engine_id, rpc_port },
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
        let (mut state, root) = lifecycle_state();
        let engine_id = EngineId(Uuid::from_u128(2));
        let rpc_port = 40_681;
        let continuation = FleetSpawnContinuation { engine_id, rpc_port };
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
            .settle_pending_spawn(continuation.clone(), ProxySpawnOutcome::Applied(MailboxId(0x4068_0002)))
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
            state
                .settle_pending_spawn(continuation, ProxySpawnOutcome::Rejected("stale owner result".to_owned()))
                .is_none(),
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
        let (mut state, root) = lifecycle_state();
        let engine_id = EngineId(Uuid::from_u128(3));
        let rpc_port = 40_682;
        state.pending_engines.insert(engine_id, PendingEngine { rpc_port, early_death: None });

        let reply = state
            .settle_pending_spawn(
                FleetSpawnContinuation { engine_id, rpc_port },
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
        let stand_in = dir.join("aether-substrate-headless");
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
