// End-to-end test for the engines cap (issue 763 P4).
//
// Boots a passive chassis hosting `FleetServer`, mails it a
// `SpawnEngine` pointed at the real `aether-headless`
// binary, and asserts the full lifecycle: the substrate forks and
// binds its RPC port, the per-engine proxy bridges the startup gap
// and connects, `ListEngines` reflects the live engine, and
// `TerminateEngine` shuts it down. This is the only test exercising
// the fork+exec + startup-race-retry + real-process path — the
// `FleetServer` unit tests cover the error arms in-process.

// Integration test resolves the server/sink actor mailboxes by their NAMESPACE
// for fixture wiring — reference id derivation, not sibling-cap addressing.
#![allow(clippy::disallowed_methods)]

use aether_actor::Addressable;
use aether_data::{Kind, MailboxId, Uuid, mailbox_id_from_name, mailbox_id_from_path};
use aether_fleet::{FleetConfig, FleetProxy, FleetServer};
use aether_kinds::descriptors;
use aether_kinds::trace::Nanos;
use aether_kinds::{
    BinarySelector, DeathReason, ListComponentBinaries, ListComponentBinariesResult, ListEngineBinaries,
    ListEngineBinariesResult, ListEngines, ListEnginesResult, SetArtifactPinned, SetArtifactPinnedResult, SpawnEngine,
    SpawnEngineResult, TerminateEngine, TerminateEngineResult, UploadBinary, UploadBinaryResult, UploadComponent,
    UploadComponentResult,
};
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::content_store::{ContentStore, EvictionPolicy};
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::{MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::mail::{Mail, MailId, MailRef, Source, SourceAddr};
use aether_substrate::testing::{TestChassis, boot_authority};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Reply sink config: records the latest reply of each engines-cap reply
// kind into shared cells. Lives at module root always-on (it names no
// `aether_substrate` type) and is the cap's `Config`.
#[derive(Clone, Default)]
pub struct ReplyCells {
    pub list: Arc<Mutex<Option<ListEnginesResult>>>,
    pub spawn: Arc<Mutex<Option<SpawnEngineResult>>>,
    pub spawn_correlation: Arc<Mutex<Option<u64>>>,
    pub terminate: Arc<Mutex<Option<TerminateEngineResult>>>,
    pub upload_binary: Arc<Mutex<Option<UploadBinaryResult>>>,
    pub upload_component: Arc<Mutex<Option<UploadComponentResult>>>,
    pub list_binaries: Arc<Mutex<Option<ListEngineBinariesResult>>>,
    pub list_components: Arc<Mutex<Option<ListComponentBinariesResult>>>,
    pub set_pinned: Arc<Mutex<Option<SetArtifactPinnedResult>>>,
}

/// Test-only reply sink registered at `aether.fleet.test.reply_sink`,
/// recording the latest reply of each engines-cap reply kind into the
/// shared [`ReplyCells`]. A field-bearing test actor, so it stays the
/// un-split `type State = Self` shape (ADR-0122), matching the cap's own
/// `proxy::sinks` fixtures — this crate carries no `runtime` feature for
/// a split shape to gate on.
pub struct ReplySink {
    cells: ReplyCells,
}

use aether_actor::actor;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};

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
        *self.cells.list.lock().expect("test setup: list cell mutex is never poisoned") = Some(reply);
    }

    #[handler::single]
    fn on_spawn_result(&mut self, ctx: &mut NativeCtx<'_>, reply: SpawnEngineResult) {
        *self.cells.spawn_correlation.lock().expect("test setup: spawn correlation cell is never poisoned") =
            Some(ctx.reply_target().correlation_id);
        *self.cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned") = Some(reply);
    }

    #[handler::single]
    fn on_terminate_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: TerminateEngineResult) {
        *self.cells.terminate.lock().expect("test setup: terminate cell mutex is never poisoned") = Some(reply);
    }

    #[handler::single]
    fn on_upload_binary_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: UploadBinaryResult) {
        *self.cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned") = Some(reply);
    }

    #[handler::single]
    fn on_upload_component_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: UploadComponentResult) {
        *self.cells.upload_component.lock().expect("test setup: upload_component cell mutex is never poisoned") =
            Some(reply);
    }

    #[handler::single]
    fn on_list_engine_binaries_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: ListEngineBinariesResult) {
        *self.cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned") = Some(reply);
    }

    #[handler::single]
    fn on_list_component_binaries_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: ListComponentBinariesResult) {
        *self.cells.list_components.lock().expect("test setup: list_components cell mutex is never poisoned") =
            Some(reply);
    }

    #[handler::single]
    fn on_set_artifact_pinned_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: SetArtifactPinnedResult) {
        *self.cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned") = Some(reply);
    }
}

fn boot(engine_config: FleetConfig) -> (Arc<Registry>, PassiveChassis<TestChassis>, Arc<Mailer>, ReplyCells) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&boot_authority(), d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let cells = ReplyCells::default();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<FleetServer>((), engine_config)
        .with_actor::<ReplySink>(cells.clone())
        .build_passive()
        .expect("caps boot");
    (registry, chassis, mailer, cells)
}

/// Distinctive path component planted in the private temp root so a leaked
/// host destination, store source, or fleet scratch path is obvious in
/// spawn-failure details.
const SENTINEL_HOST_PATH: &str = "SENTINEL_HOST_PATH_5499";

/// `--app-name` filename `prepare_fork` would materialize under. A leaked
/// `exec_path` includes this basename.
const SENTINEL_BASENAME: &str = "SentinelAppName5499";

/// Persist an inert binary through the public content-store core so
/// `FleetServer::init` restores it. The metadata is the fleet
/// `StoredEntry` JSON shape; this is not a `--describe`d chassis.
/// The seeder handle (and its `lock.pid`) drops before this returns so the
/// cap can open the same root.
fn write_inert_binary_store(store_dir: &Path, bytes: &[u8]) -> String {
    ContentStore::<serde_json::Value>::open(&store_dir.join("v1"), EvictionPolicy::None)
        .expect("test setup: open inert content store")
        .upload(
            bytes,
            serde_json::json!({
                "kind": "Binary",
                "manifest": {
                    "Binary": {
                        "chassis": "headless",
                        "caps": ["aether.rpc.server"],
                        "git_sha": "deadbee",
                        "profile": "debug",
                        "target": "x86_64-unknown-linux-gnu",
                        "env_keys": ["AETHER_RPC_PORT"],
                        "argv_flags": ["rpc-port"]
                    }
                }
            }),
            Some("headless".into()),
        )
        .expect("test setup: persist inert binary")
}

fn inert_store_config(store_dir: &Path, engine_root: &Path) -> FleetConfig {
    FleetConfig {
        binary_store_dir: Some(store_dir.to_string_lossy().into_owned()),
        fleet_store_root: Some(engine_root.to_string_lossy().into_owned()),
        binary_bootstrap: HashSet::new(),
        ..FleetConfig::default()
    }
}

fn hash_selector(hash: &str) -> BinarySelector {
    BinarySelector { query: Some(hash.to_owned()), chassis: None, caps: vec![], target: None }
}

/// Isolated store + engine-root config with an explicit disk budget and
/// no bootstrap ingest — used by operator-pin handler tests so an unnamed
/// `pin: true` upload is not also name-protected.
fn pin_store_config(store_dir: &Path, engine_root: &Path, budget: u64) -> FleetConfig {
    FleetConfig {
        binary_store_dir: Some(store_dir.to_string_lossy().into_owned()),
        fleet_store_root: Some(engine_root.to_string_lossy().into_owned()),
        binary_bootstrap: HashSet::new(),
        binary_disk_budget_bytes: budget,
        ..FleetConfig::default()
    }
}

fn history_binaries() -> ListEngineBinaries {
    ListEngineBinaries { chassis: None, caps: Vec::new(), target: None, limit: None, include_history: true }
}

fn history_components() -> ListComponentBinaries {
    ListComponentBinaries { namespace: None, handled_kind: None, limit: None, include_history: true }
}

fn owned_sidecar_path(store_dir: &Path, hash: &str) -> PathBuf {
    store_dir.join("v1").join("entries").join(format!("{hash}.manifest"))
}

fn occupy_owned_sidecar_as_dir(store_dir: &Path, hash: &str) {
    let path = owned_sidecar_path(store_dir, hash);
    if path.is_file() {
        fs::remove_file(&path).expect("remove the owned sidecar file");
    }
    fs::create_dir_all(&path).expect("owned sidecar path is a directory");
}

fn assert_path_free_persist_error(error: &str, operation: &str, store_dir: &Path) {
    assert!(error.contains(operation), "persistence error must name the operation {operation:?}: {error}");
    assert!(
        error.contains("os error")
            || error.contains("IsADirectory")
            || error.contains("AlreadyExists")
            || error.contains("Directory"),
        "persistence error must carry an IO category: {error}"
    );
    let store = store_dir.to_string_lossy();
    assert!(!error.contains(store.as_ref()), "persistence error must not include the store path: {error}");
    assert!(!error.contains("/v1/entries/"), "persistence error must not include a store sidecar path: {error}");
}

#[cfg(unix)]
fn write_pressure_bin(path: &Path, token: &str) {
    use std::os::unix::fs::PermissionsExt;
    let mut script = String::from("#!/bin/sh\nif [ \"$1\" = \"--describe\" ]; then printf '");
    script.push_str("{\"chassis\":\"headless\",\"caps\":[\"aether.rpc.server\"],\"git_sha\":\"");
    script.push_str(token);
    script.push_str("\",\"profile\":\"debug\",\"target\":\"x86_64-unknown-linux-gnu\",\"env_keys\":[\"AETHER_RPC_PORT\"],\"argv_flags\":[\"rpc-port\"]}'; fi\n");
    fs::write(path, script).expect("write pressure stand-in");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod pressure stand-in");
}

/// Build the engines-cap config that isolates the hub binary store
/// (ADR-0115) under `store_dir` and bootstraps it with the `headless` bin,
/// so the cap resolves a `default` selector to that binary (issue 1954),
/// and isolates the per-engine spawn-dir parent (issue 1274) under
/// `engine_root`. Both dirs ride `FleetConfig` (ADR-0090) instead of an
/// env side-channel; the heartbeat stays disabled (the `Default`).
/// `FleetServer::init` forks `<headless> --describe` to ingest the
/// binary store and resolves `engine_root` into
/// `FleetServerState::fleet_store_root` — a per-run dir instead of the
/// shared default (`~/.local/share/aether/engines`), which would collide
/// with any sibling test, leaked orphan, or live MCP engine on id
/// `0…01`.
fn bootstrap_store_config(store_dir: &Path, engine_root: &Path, headless: &str) -> FleetConfig {
    FleetConfig {
        binary_store_dir: Some(store_dir.to_string_lossy().into_owned()),
        fleet_store_root: Some(engine_root.to_string_lossy().into_owned()),
        binary_bootstrap: HashSet::from([headless.to_owned()]),
        ..FleetConfig::default()
    }
}

/// The `default` registry selector — empty `query`, no attribute filters —
/// the bare-spawn form that resolves to the bootstrapped headless bin.
fn default_selector() -> BinarySelector {
    BinarySelector { query: None, chassis: None, caps: vec![], target: None }
}

/// Drive one request kind at `aether.fleet`, reply-to the sink, and
/// block until `probe` returns a recorded reply (or `deadline` passes).
fn drive<K: Kind, T>(mailer: &Arc<Mailer>, request: &K, deadline: Duration, probe: impl Fn() -> Option<T>) -> T {
    let server = mailbox_id_from_name(<FleetServer as Addressable>::NAMESPACE);
    let sink = mailbox_id_from_name(<ReplySink as Addressable>::NAMESPACE);
    mailer.push(
        Mail::new(server, K::ID, request.encode_into_bytes(), 1)
            .with_reply_to(Source::with_correlation(SourceAddr::Component(sink), 1)),
    );
    wait_for(deadline, probe)
}

fn wait_for<T>(deadline: Duration, probe: impl Fn() -> Option<T>) -> T {
    let until = Instant::now() + deadline;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < until, "no reply within {deadline:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

/// Inject one request with an explicit trace root. This pins the deferred
/// reply's correlation and lets the test prove that the original settlement
/// stays open through the later staged-spawn task turn.
fn enqueue_with_root<K: Kind>(registry: &Registry, mailer: &Mailer, request: &K, root: MailId, correlation_id: u64) {
    let server = registry.lookup(FleetServer::NAMESPACE).expect("fleet server mailbox registered");
    let sink = registry.lookup(ReplySink::NAMESPACE).expect("reply sink mailbox registered");
    mailer.record_sent(root, root, None, root.sender, server, K::ID);
    let MailboxEntry::Inbox { handler, .. } = registry.entry(server).expect("fleet server route exists") else {
        panic!("fleet server route is an inbox");
    };
    handler.enqueue(OwnedDispatch::disarmed(
        K::ID,
        None,
        Source::with_correlation(SourceAddr::Component(sink), correlation_id),
        MailRef::from(request.encode_into_bytes()),
        1,
        root,
        root,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
}

fn register_proxy_collision(registry: &Registry, engine_id: Uuid) -> MailboxId {
    let canonical_name = format!("{}/{}:{}", FleetServer::NAMESPACE, FleetProxy::NAMESPACE, engine_id.simple());
    let mailbox_id = mailbox_id_from_path(&canonical_name);
    registry
        .try_register_inbox_with_id(
            &boot_authority(),
            mailbox_id,
            &canonical_name,
            Arc::new(|dispatch: OwnedDispatch| dispatch.discharge()),
        )
        .expect("install test-only proxy collision authority");
    mailbox_id
}

fn assert_port_closes(rpc_port: u16) {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Err(_) => return,
            Ok(stream) => drop(stream),
        }
        assert!(Instant::now() < deadline, "rolled-back proxy child still listens on {address}");
        thread::sleep(Duration::from_millis(25));
    }
}

/// RAII guard that best-effort terminates a spawned engine on drop so a
/// panic between spawn and the explicit terminate doesn't leave the forked
/// headless substrate child running. Disarm with [`EngineReaper::disarm`]
/// once the engine is explicitly terminated; the guard then no-ops on drop
/// (a double-terminate is harmless but wastes a round trip on the happy path).
struct EngineReaper {
    mailer: Arc<Mailer>,
    cells: ReplyCells,
    engine_id: Option<String>,
}

impl EngineReaper {
    fn disarm(&mut self) {
        self.engine_id = None;
    }
}

impl Drop for EngineReaper {
    fn drop(&mut self) {
        let Some(engine_id) = self.engine_id.take() else {
            return;
        };
        let server = mailbox_id_from_name(<FleetServer as Addressable>::NAMESPACE);
        let sink = mailbox_id_from_name(<ReplySink as Addressable>::NAMESPACE);
        self.mailer.push(
            Mail::new(server, TerminateEngine::ID, TerminateEngine { engine_id }.encode_into_bytes(), 1)
                .with_reply_to(Source::with_correlation(SourceAddr::Component(sink), 1)),
        );
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if self.cells.terminate.lock().ok().and_then(|mut g| g.take()).is_some() {
                break;
            }
            if Instant::now() >= until {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

mod tests {
    use super::*;

    #[test]
    fn engines_cap_spawns_lists_and_terminates_a_real_headless_substrate() {
        // The forked headless chassis resolves through `dist/manifest.json`
        // (`cargo xtask dist` first) — `CARGO_BIN_EXE_*` only resolves inside
        // the package that defines the binary, and this suite lives in
        // `aether-fleet`, not the bundle.
        let headless = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        // Bootstrap the binary store with the headless bin so the cap
        // resolves a `default` selector to it (ADR-0115, #1954). Before
        // `boot()` — init reads the bootstrap env. Cleaned on success.
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-binstore-{}-{nanos}", process::id()));
        let root = env::temp_dir().join(format!("aether-engcap-store-{}-{nanos}", process::id()));

        let (registry, chassis, mailer, cells) = boot(bootstrap_store_config(&store_dir, &root, &headless));

        // Spawn: the cap assigns a port, forks the substrate, and the proxy
        // retries the dial until the fresh process binds. The explicit root
        // proves the manual handler retains the original correlation and
        // settlement through owner apply and the later task turn.
        let correlation_id = 0x4068;
        let root_mail = MailId::new(MailboxId(0x4068_5A6E), correlation_id);
        let settled = chassis.settlement_registry().subscribe_settlement(root_mail);
        enqueue_with_root(
            &registry,
            &mailer,
            &SpawnEngine { selector: default_selector(), args: vec![], boot_manifest: None },
            root_mail,
            correlation_id,
        );
        let spawn = wait_for(Duration::from_secs(30), || {
            cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").take()
        });
        assert_eq!(
            cells.spawn_correlation.lock().expect("test setup: spawn correlation cell is never poisoned").take(),
            Some(correlation_id),
            "the deferred reply keeps the originating correlation",
        );
        let engine_id = match spawn {
            SpawnEngineResult::Ok { engine_id, rpc_port } => {
                assert_ne!(rpc_port, 0, "cap should report the assigned RPC port");
                engine_id
            }
            SpawnEngineResult::Err { error, .. } => panic!("spawn failed: {error}"),
        };
        settled
            .recv_timeout(Duration::from_secs(5))
            .expect("the original root settles after the staged reply is delivered");
        let mut reaper =
            EngineReaper { mailer: Arc::clone(&mailer), cells: cells.clone(), engine_id: Some(engine_id.clone()) };

        // List: the freshly-spawned engine shows up in the cap's table.
        let list = drive(&mailer, &ListEngines {}, Duration::from_secs(5), || {
            cells.list.lock().expect("test setup: list cell mutex is never poisoned").take()
        });
        assert!(
            list.engines.iter().any(|e| e.engine_id == engine_id),
            "spawned engine {engine_id} should appear in ListEngines: {list:?}",
        );

        // Terminate: the cap forwards to the proxy, which SIGKILLs the
        // substrate and self-shuts-down; the table entry is dropped.
        let terminate =
            drive(&mailer, &TerminateEngine { engine_id: engine_id.clone() }, Duration::from_secs(5), || {
                cells.terminate.lock().expect("test setup: terminate cell mutex is never poisoned").take()
            });
        assert!(
            matches!(terminate, TerminateEngineResult::Ok),
            "terminate of a live engine should succeed: {terminate:?}",
        );
        reaper.disarm();

        // After terminate, the engine is gone from the table.
        let list_after = drive(&mailer, &ListEngines {}, Duration::from_secs(5), || {
            cells.list.lock().expect("test setup: list cell mutex is never poisoned").take()
        });
        assert!(
            !list_after.engines.iter().any(|e| e.engine_id == engine_id),
            "terminated engine {engine_id} should be gone from ListEngines: {list_after:?}",
        );

        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&root);
    }

    /// Scheduler-backed authoritative rejection: `FleetProxy::init` connects
    /// to a real headless child, but a test-only canonical route owns the
    /// would-be proxy name when the registry owner applies the birth. The
    /// prepared proxy state must roll back before the single id-bearing reply,
    /// leaving no live row and no process still listening on its assigned port.
    #[test]
    fn owner_rejected_staged_proxy_replies_once_and_reaps_the_child() {
        let headless = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-rejected-store-{}-{nanos}", process::id()));
        let root = env::temp_dir().join(format!("aether-engcap-rejected-engine-{}-{nanos}", process::id()));
        let (registry, chassis, mailer, cells) = boot(bootstrap_store_config(&store_dir, &root, &headless));

        let expected_engine = Uuid::from_u128(1);
        let collision = register_proxy_collision(&registry, expected_engine);
        let correlation_id = 0x4068_C011;
        let root_mail = MailId::new(MailboxId(0x4068_C011), correlation_id);
        let settled = chassis.settlement_registry().subscribe_settlement(root_mail);
        enqueue_with_root(
            &registry,
            &mailer,
            &SpawnEngine { selector: default_selector(), args: vec![], boot_manifest: None },
            root_mail,
            correlation_id,
        );

        let rejected = wait_for(Duration::from_secs(30), || {
            cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").take()
        });
        assert_eq!(
            cells.spawn_correlation.lock().expect("test setup: spawn correlation cell is never poisoned").take(),
            Some(correlation_id),
        );
        let engine_id = match rejected {
            SpawnEngineResult::Err { engine_id: Some(engine_id), error } => {
                assert_eq!(engine_id, expected_engine.to_string());
                assert!(error.contains("proxy activation failed"), "unexpected apply error: {error}");
                engine_id
            }
            other => panic!("owner collision must produce one id-bearing spawn failure, got {other:?}"),
        };
        settled.recv_timeout(Duration::from_secs(5)).expect("the rejected staged reply releases the original root");
        thread::sleep(Duration::from_millis(100));
        assert!(
            cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").is_none(),
            "owner rejection emits exactly one spawn result",
        );

        let list = drive(&mailer, &ListEngines {}, Duration::from_secs(5), || {
            cells.list.lock().expect("test setup: list cell mutex is never poisoned").take()
        });
        assert!(list.engines.is_empty(), "a rejected reservation never becomes publicly live: {list:?}");
        let record = list
            .recently_died
            .iter()
            .find(|record| record.engine_id == engine_id)
            .unwrap_or_else(|| panic!("rejected spawn {engine_id} leaves one death record: {list:?}"));
        assert!(matches!(record.reason, DeathReason::SpawnFailed { .. }));
        assert_port_closes(record.rpc_port);

        registry.drop_mailbox(&boot_authority(), collision).expect("remove test-only proxy collision authority");
        drop(chassis);
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&root);
    }

    /// A spawn that forks a substrate which never binds its RPC port
    /// fails after the connect budget, and that failure leaves an
    /// observable trail: the `Err` carries the allocated `engine_id`,
    /// and a subsequent `ListEngines` shows a `recently_died` entry with
    /// reason `SpawnFailed` whose `engine_id` matches (issue 2423).
    ///
    /// Tripwire: a genuinely-failed spawn must surface an id-bearing
    /// `Err` and a `SpawnFailed` `recently_died` record — without the
    /// surfacing, the error carries no id (`engine_id: None`) and the
    /// failure never reaches the ring, so a caller can't correlate and
    /// reap the orphan.
    #[cfg(unix)]
    #[test]
    fn failed_spawn_surfaces_engine_id_and_records_spawn_failed() {
        use std::os::unix::fs::PermissionsExt;

        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-engcap-badspawn-{}-{nanos}", process::id()));
        fs::create_dir_all(&dir).expect("test setup: bad-spawn temp dir");

        // A stand-in chassis bin that ingests cleanly (prints a
        // *conforming* headless manifest on `--describe` — non-empty
        // caps + config surface, so the upload gate accepts it, #3936)
        // but, when forked normally, `exec`s a sleep instead of binding
        // the RPC port the hub hands it via `--rpc-port` (ADR-0162 argv
        // injection). The proxy's dial refuses for the whole (short)
        // connect budget, so the spawn fails after the substrate forked
        // but never connected — the post-allocation failure this test
        // pins. `exec` makes the sleep the direct child so the proxy's
        // SIGKILL reaps it (no orphan).
        let stand_in = dir.join("aether-headless");
        fs::write(
            &stand_in,
            "#!/bin/sh\nif [ \"$1\" = \"--describe\" ]; then printf \
                 '{\"chassis\":\"headless\",\"caps\":[\"aether.rpc.server\"],\
                 \"git_sha\":\"deadbee\",\"profile\":\"debug\",\
                 \"target\":\"x86_64-unknown-linux-gnu\",\
                 \"env_keys\":[\"AETHER_RPC_PORT\"],\"argv_flags\":[\"rpc-port\"]}'; \
                 exit 0; fi\n\
                 exec sleep 30\n",
        )
        .expect("test setup: write bad-spawn stand-in");
        fs::set_permissions(&stand_in, fs::Permissions::from_mode(0o755))
            .expect("test setup: chmod bad-spawn stand-in");

        let store_dir = dir.join("store");
        let root = dir.join("engines");

        // A short connect budget so the doomed dial fails quickly rather
        // than burning the default 30 s. `fleet_store_root` isolates this
        // run's per-engine spawn-dir parent (issue 1274) from the shared
        // default, which would collide with any sibling test, leaked
        // orphan, or live MCP engine on id `0…01`.
        let config = FleetConfig {
            binary_store_dir: Some(store_dir.to_string_lossy().into_owned()),
            fleet_store_root: Some(root.to_string_lossy().into_owned()),
            binary_bootstrap: HashSet::from([stand_in.to_string_lossy().into_owned()]),
            proxy_connect_budget_secs: 2,
            ..FleetConfig::default()
        };
        let (_registry, _chassis, mailer, cells) = boot(config);

        // The spawn forks the stand-in, the proxy dials for the 2 s
        // budget, then the cap returns Err. Deadline comfortably over
        // the budget + fork.
        let spawn = drive(
            &mailer,
            &SpawnEngine { selector: default_selector(), args: vec![], boot_manifest: None },
            Duration::from_secs(20),
            || cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").take(),
        );
        let engine_id = match spawn {
            SpawnEngineResult::Err { engine_id: Some(id), error } => {
                assert!(error.contains("proxy failed to connect"), "unexpected error: {error}");
                id
            }
            other => panic!("expected an id-bearing spawn Err, got {other:?}"),
        };

        // The failure is recorded as a `SpawnFailed` death keyed by the
        // same engine_id, so a caller can correlate and reap.
        let list = drive(&mailer, &ListEngines {}, Duration::from_secs(5), || {
            cells.list.lock().expect("test setup: list cell mutex is never poisoned").take()
        });
        assert!(
            !list.engines.iter().any(|e| e.engine_id == engine_id),
            "a failed spawn must not register a live engine: {list:?}",
        );
        let record = list
            .recently_died
            .iter()
            .find(|d| d.engine_id == engine_id)
            .unwrap_or_else(|| panic!("failed spawn {engine_id} must leave a recently_died entry: {list:?}"));
        assert!(
            matches!(record.reason, DeathReason::SpawnFailed { .. }),
            "a failed spawn must be recorded as SpawnFailed, got {:?}",
            record.reason,
        );

        let _ = fs::remove_dir_all(&dir);
    }

    fn inert_spawn(hash: &str) -> SpawnEngine {
        SpawnEngine {
            selector: hash_selector(hash),
            args: vec!["--app-name".to_owned(), SENTINEL_BASENAME.to_owned()],
            boot_manifest: None,
        }
    }

    fn assert_path_free_spawn_failure(
        spawn: SpawnEngineResult,
        list: &ListEnginesResult,
        phase: &str,
        hash: &str,
        sentinel_root: &Path,
    ) {
        let (engine_id, error) = match spawn {
            SpawnEngineResult::Err { engine_id: Some(id), error } => (id, error),
            other => panic!("expected an id-bearing spawn Err, got {other:?}"),
        };
        assert_eq!(engine_id, Uuid::from_u128(1).to_string(), "the first allocated id is correlatable");
        let prefix = format!("{phase} binary {hash}: ");
        assert!(error.starts_with(&prefix), "detail must retain phase and hash: {error}");
        let category = error.strip_prefix(&prefix).expect("prefix checked");
        assert!(!category.is_empty(), "detail must retain an IO category: {error}");
        assert!(!category.contains('/') && !category.contains('\\'), "IO category must not embed a host path: {error}");
        let sentinel_root = sentinel_root.to_string_lossy();
        assert!(!error.contains(sentinel_root.as_ref()), "detail must not leak the sentinel root: {error}");
        assert!(!error.contains(SENTINEL_HOST_PATH), "detail must not leak the sentinel path component: {error}");
        assert!(!error.contains(SENTINEL_BASENAME), "detail must not leak the app-name filename: {error}");
        assert!(list.engines.is_empty(), "a failed spawn must not register a live engine: {list:?}");
        let deaths: Vec<_> = list.recently_died.iter().filter(|record| record.engine_id == engine_id).collect();
        assert_eq!(deaths.len(), 1, "the allocated id must leave exactly one death record: {list:?}");
        match &deaths[0].reason {
            DeathReason::SpawnFailed { detail } => {
                assert_eq!(detail, &error, "the ring must carry the same detail as the spawn Err");
            }
            other => panic!("a failed spawn must be recorded as SpawnFailed, got {other:?}"),
        }
    }

    /// Tripwire: `prepare_fork`'s materialize `map_err` used to format
    /// `exec_path` into `SpawnFailed.detail`. Blocking the per-engine dest
    /// dir with an owned file fails realize without a process or a
    /// permission/root dependency; the outward detail must keep phase,
    /// hash, and IO category and omit the host path (issue 5499).
    #[test]
    fn materialize_failure_is_id_bearing_and_path_free() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir =
            env::temp_dir().join(format!("aether-engcap-materialize-{SENTINEL_HOST_PATH}-{}-{nanos}", process::id()));
        let store_dir = dir.join("store");
        let root = dir.join("engines");
        let hash = write_inert_binary_store(&store_dir, b"aether-issue-5499-inert-bytes");
        let (_registry, chassis, mailer, cells) = boot(inert_store_config(&store_dir, &root));

        fs::create_dir_all(&root).expect("test setup: fleet store root");
        fs::write(root.join(Uuid::from_u128(1).simple().to_string()), b"owned-regular-file-blocking-engine-dir")
            .expect("test setup: block the per-engine dest dir with a regular file");

        let spawn = drive(&mailer, &inert_spawn(&hash), Duration::from_secs(10), || {
            cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").take()
        });
        let list = drive(&mailer, &ListEngines {}, Duration::from_secs(5), || {
            cells.list.lock().expect("test setup: list cell mutex is never poisoned").take()
        });
        assert_path_free_spawn_failure(spawn, &list, "materializing", &hash, &dir);

        drop(chassis);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The per-engine scratch dirs currently under an engine store root —
    /// the uuid-named dirs `prepare_fork` materializes a binary into.
    fn engine_dirs_under(root: &Path) -> Vec<String> {
        let Ok(entries) = fs::read_dir(root) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| Uuid::parse_str(name).is_ok())
            .collect()
    }

    /// Every spawn copies the resolved chassis binary to
    /// `<engine store root>/<engine id>/<app name>` before it forks, and
    /// nothing in production ever removed one — on a long-lived hub that is
    /// a disk leak proportional to spawn count, under bare uuids with no
    /// shared prefix to sweep by hand. This drives the whole real path: the
    /// store resolves, `prepare_fork` materializes, process creation fails
    /// on the absent interpreter, and the terminal `SpawnFailed` has to
    /// reclaim what was already written (issue 5502).
    #[test]
    fn a_failed_spawn_reclaims_the_binary_it_materialized() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-engcap-reap-{}-{nanos}", process::id()));
        let store_dir = dir.join("store");
        let root = dir.join("engines");
        let bytes = format!("#!{}\n", dir.join("missing").display());
        let hash = write_inert_binary_store(&store_dir, bytes.as_bytes());
        let (_registry, chassis, mailer, cells) = boot(inert_store_config(&store_dir, &root));

        let spawn = drive(&mailer, &inert_spawn(&hash), Duration::from_secs(10), || {
            cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").take()
        });
        assert!(
            matches!(spawn, SpawnEngineResult::Err { engine_id: Some(_), .. }),
            "the fork must fail after the materialize for this to prove anything: {spawn:?}",
        );
        assert!(
            engine_dirs_under(&root).is_empty(),
            "a failed spawn leaves no materialized binary behind: {:?}",
            engine_dirs_under(&root),
        );

        drop(chassis);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Reaping at each death cannot cover a hub killed outright, or a host
    /// that refuses to unlink a running image, so a fresh hub sweeps what
    /// an earlier one left under its store root. It must reclaim only its
    /// own engine dirs: the root is operator-settable and can share a
    /// directory with anything, including the sibling artifact store.
    #[test]
    fn boot_sweeps_engine_dirs_an_earlier_hub_left_behind() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-engcap-sweep-{}-{nanos}", process::id()));
        let root = dir.join("engines");
        let leftover = root.join(Uuid::from_u128(0xDEAD).simple().to_string());
        let bystander = root.join("not-an-engine-dir");
        fs::create_dir_all(&leftover).expect("test setup: a leftover engine dir");
        fs::create_dir_all(&bystander).expect("test setup: a bystander dir sharing the root");
        fs::write(leftover.join("substrate"), b"materialized-by-a-hub-that-is-gone")
            .expect("test setup: the leftover materialized binary");

        let (_registry, chassis, _mailer, _cells) = boot(inert_store_config(&dir.join("store"), &root));

        assert!(!leftover.exists(), "a fresh hub reclaims the engine dirs an earlier one left");
        assert!(bystander.exists(), "the sweep touches only the dirs the cap itself creates");

        drop(chassis);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Tripwire: `prepare_fork`'s `Command::spawn` `map_err` used to format
    /// `exec_path` into `SpawnFailed.detail`. Bare non-executable text hits
    /// the platform ENOEXEC shell fallback and becomes a later child-exit;
    /// a shebang whose interpreter is an absent path under this test's
    /// private temp dir fails process creation directly (no live child, no
    /// proxy-connect budget). The outward detail must keep phase, hash, and
    /// IO category and omit the host path (issue 5499).
    #[test]
    fn process_spawn_failure_is_id_bearing_and_path_free() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-engcap-exec-{SENTINEL_HOST_PATH}-{}-{nanos}", process::id()));
        let store_dir = dir.join("store");
        let root = dir.join("engines");
        let interpreter = dir.join("missing");
        assert!(
            !interpreter.exists(),
            "test setup: missing-interpreter path must not exist: {}",
            interpreter.display(),
        );
        let bytes = format!("#!{}\n", interpreter.display());
        let hash = write_inert_binary_store(&store_dir, bytes.as_bytes());
        let (_registry, chassis, mailer, cells) = boot(inert_store_config(&store_dir, &root));

        let spawn = drive(&mailer, &inert_spawn(&hash), Duration::from_secs(10), || {
            cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").take()
        });
        let list = drive(&mailer, &ListEngines {}, Duration::from_secs(5), || {
            cells.list.lock().expect("test setup: list cell mutex is never poisoned").take()
        });
        assert_path_free_spawn_failure(spawn, &list, "spawning", &hash, &dir);

        drop(chassis);
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Live restart-supervision coverage (the `restart_on_crash` path).
///
/// These two are the only tests that drive a *committed* engine through a
/// real crash and out the other side, so they are what fails if
/// `consider_restart` never fires, if the re-fork loses the recipe, or if
/// the burst limit does not actually end a crash loop. The reducer tests
/// in `server/mod.rs` cover the decision in isolation; nothing there forks
/// a process, so nothing there can catch a restart that decides correctly
/// and then re-forks wrong.
#[cfg(unix)]
mod restart_supervision {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// How long a crashing generation stays up before it kills its real
    /// substrate. It has to outlast the spawn-commit boundary — a death
    /// observed while the birth is still pending latches as a failed
    /// spawn and is deliberately *not* restarted — so this is sized for
    /// a debug fork under parallel build load rather than for speed.
    const FIXTURE_ALIVE_SECS: u64 = 3;

    /// Settle time the cap waits before re-forking. Short, because the
    /// test's patience lives in the deadlines below, not here.
    const RESTART_BACKOFF_MILLIS: u64 = 250;

    /// How long to wait for a successor engine to appear. Generous by
    /// construction: a restart costs a crash window, a backoff, a fork,
    /// and a fresh chassis boot, and every one of those stretches under a
    /// loaded machine. A tight budget here would fail the test for being
    /// slow rather than for being wrong (issue 5498).
    const SUCCESSOR_DEADLINE_SECS: u64 = 90;

    /// The same patience, extended across a whole crash loop — several
    /// generations, each paying the full cost above.
    const CRASH_LOOP_DEADLINE_SECS: u64 = 150;

    /// After the burst limit is reached, how long to keep watching for a
    /// restart that must never come. Comfortably longer than one full
    /// generation, so a cap that kept restarting would be caught rather
    /// than merely be slow enough to look stopped.
    const LOOP_SETTLE_SECS: u64 = 20;

    /// Poll cadence for the list-driven waits.
    const POLL_MILLIS: u64 = 250;

    /// How long the first generation gets to fork, boot a real chassis,
    /// and reply. Sized like the neighbouring real-fork suites' spawn
    /// waits rather than trimmed, for the same load reason.
    const FIRST_SPAWN_DEADLINE_SECS: u64 = 60;

    /// A stand-in chassis binary that comes up for real and then crashes.
    ///
    /// It has to be a real substrate to be restarted at all: the engine
    /// must connect, commit, and be supervised before its death counts as
    /// a `Crashed` eviction rather than a failed spawn. So the script
    /// delegates the work to the genuine headless binary and adds only
    /// two things a real chassis cannot do — it records the argv the hub
    /// handed it, and it kills its own substrate on a timer.
    ///
    /// `--describe` is delegated verbatim so the store ingests a genuine
    /// conforming manifest and a `default` selector resolves to it.
    ///
    /// Only `--rpc-port` is forwarded to the real binary. The other flags
    /// exist to be *observed* in the argv log — they are recipe-fidelity
    /// markers, and feeding a real chassis flags it does not define would
    /// test clap rather than the cap. Extracting the port by scanning for
    /// its flag is also what makes the log meaningful: the script never
    /// assumes a position the cap might have changed.
    ///
    /// When `once_marker` is set, only the first generation crashes and
    /// every later one runs straight through — one restart, then a stable
    /// successor. Without it every generation crashes, which is the crash
    /// loop the burst limit has to stop.
    fn write_crashing_stand_in(path: &Path, real: &str, argv_log: &Path, once_marker: Option<&Path>) {
        let crash_guard = once_marker.map_or_else(String::new, |marker| {
            format!(
                "if [ -f '{}' ]; then exec \"$REAL\" --rpc-port \"$port\"; fi\n: > '{}'\n",
                marker.display(),
                marker.display()
            )
        });
        let script = format!(
            "#!/bin/sh\n\
             REAL='{real}'\n\
             if [ \"$1\" = \"--describe\" ]; then exec \"$REAL\" --describe; fi\n\
             echo \"$*\" >> '{argv_log}'\n\
             port=''\n\
             prev=''\n\
             for a in \"$@\"; do\n\
             \x20 if [ \"$prev\" = \"--rpc-port\" ]; then port=\"$a\"; fi\n\
             \x20 prev=\"$a\"\n\
             done\n\
             {crash_guard}\
             \"$REAL\" --rpc-port \"$port\" &\n\
             child=$!\n\
             sleep {FIXTURE_ALIVE_SECS}\n\
             kill -9 \"$child\" 2>/dev/null\n\
             exit 1\n",
            argv_log = argv_log.display(),
        );
        fs::write(path, script).expect("test setup: write crashing stand-in");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("test setup: chmod crashing stand-in");
    }

    /// The engines-cap config for a restart test: the isolated store and
    /// spawn-dir parent the other suites use, plus an armed restart policy.
    fn restart_config(store_dir: &Path, engine_root: &Path, stand_in: &Path, burst_limit: u32) -> FleetConfig {
        FleetConfig {
            binary_store_dir: Some(store_dir.to_string_lossy().into_owned()),
            fleet_store_root: Some(engine_root.to_string_lossy().into_owned()),
            binary_bootstrap: HashSet::from([stand_in.to_string_lossy().into_owned()]),
            restart_on_crash: true,
            restart_backoff_millis: RESTART_BACKOFF_MILLIS,
            restart_burst_limit: burst_limit,
            ..FleetConfig::default()
        }
    }

    /// Re-drive `ListEngines` until `probe` accepts a snapshot, or the
    /// deadline passes. The restart path produces no reply of its own —
    /// it owes nobody one — so the cap's public list is how a test
    /// observes it at all.
    fn poll_list<T>(
        mailer: &Arc<Mailer>,
        cells: &ReplyCells,
        deadline: Duration,
        probe: impl Fn(&ListEnginesResult) -> Option<T>,
    ) -> Option<T> {
        let until = Instant::now() + deadline;
        loop {
            let list = drive(mailer, &ListEngines {}, Duration::from_secs(15), || {
                cells.list.lock().expect("test setup: list cell mutex is never poisoned").take()
            });
            if let Some(found) = probe(&list) {
                return Some(found);
            }
            if Instant::now() >= until {
                return None;
            }
            thread::sleep(Duration::from_millis(POLL_MILLIS));
        }
    }

    /// Every argv line the stand-in has recorded so far, oldest first —
    /// one line per forked generation.
    fn recorded_argv(argv_log: &Path) -> Vec<String> {
        fs::read_to_string(argv_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// A crashed engine is re-forked from the recipe it was spawned with,
    /// under a fresh engine id.
    ///
    /// The whole live path in one test: a real substrate comes up, is
    /// committed, dies by SIGKILL (a genuine connection-close `Crashed`,
    /// not a `Terminated`), and a successor takes its place. Three things
    /// would break silently without it — a `consider_restart` that never
    /// reaches `restart_engine`, a re-fork that drops the caller's args or
    /// boot manifest from the retained recipe, and a successor that
    /// reuses the dead engine's id instead of minting one. The argv
    /// assertions are the recipe tripwire: the caller's flags must
    /// reappear verbatim and still lead the hub's own injections, while
    /// `--rpc-port` must differ, because a port is reserved per fork and
    /// replaying the dead one would collide.
    #[test]
    fn a_crashed_engine_is_restarted_under_a_fresh_id_from_the_same_recipe() {
        let real = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-engcap-restart-{}-{nanos}", process::id()));
        fs::create_dir_all(&dir).expect("test setup: restart temp dir");

        let argv_log = dir.join("argv.log");
        let stand_in = dir.join("aether-headless");
        write_crashing_stand_in(&stand_in, &real, &argv_log, Some(&dir.join("crashed-once")));

        let (_registry, _chassis, mailer, cells) =
            boot(restart_config(&dir.join("store"), &dir.join("engines"), &stand_in, 5));

        // Recipe-fidelity markers: the stand-in records them and forwards
        // only `--rpc-port` to the real chassis, so a restart that loses
        // either one shows up in the argv log rather than as a boot error.
        let caller_args = vec!["--fixture-marker".to_owned(), "seven".to_owned()];
        let boot_manifest = dir.join("boot-manifest.json").to_string_lossy().into_owned();
        let spawn = drive(
            &mailer,
            &SpawnEngine {
                selector: default_selector(),
                args: caller_args,
                boot_manifest: Some(boot_manifest.clone()),
            },
            Duration::from_secs(FIRST_SPAWN_DEADLINE_SECS),
            || cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").take(),
        );
        let original = match spawn {
            SpawnEngineResult::Ok { engine_id, .. } => engine_id,
            other @ SpawnEngineResult::Err { .. } => panic!("the first generation must spawn cleanly, got {other:?}"),
        };

        // The successor is any supervised engine that is not the one that
        // died — the restart mints a fresh id by design.
        let successor = poll_list(&mailer, &cells, Duration::from_secs(SUCCESSOR_DEADLINE_SECS), |list| {
            list.engines.iter().find(|e| e.engine_id != original).map(|e| e.engine_id.clone())
        })
        .unwrap_or_else(|| {
            panic!("a crashed engine must be restarted under a fresh id within {SUCCESSOR_DEADLINE_SECS}s")
        });
        let mut reaper =
            EngineReaper { mailer: Arc::clone(&mailer), cells: cells.clone(), engine_id: Some(successor.clone()) };
        assert_ne!(successor, original, "the successor is a distinct engine, not the corpse re-listed");

        // The original's death is on record, and it is a crash — the only
        // reason class restart supervision acts on.
        let died = poll_list(&mailer, &cells, Duration::from_secs(30), |list| {
            list.recently_died.iter().find(|d| d.engine_id == original).map(|d| d.reason.clone())
        })
        .unwrap_or_else(|| panic!("the crashed engine {original} must leave a death record"));
        assert!(matches!(died, DeathReason::Crashed { .. }), "a killed substrate is a crash, got {died:?}");

        // The recipe replayed intact.
        let argv = recorded_argv(&argv_log);
        assert!(argv.len() >= 2, "the restart must have forked a second generation; argv log: {argv:?}");
        let (first, second) = (&argv[0], &argv[1]);
        for (generation, line) in [("original", first), ("successor", second)] {
            assert!(
                line.contains("--fixture-marker seven"),
                "the {generation} generation must carry the caller's args: {line}",
            );
            assert!(
                line.contains(&format!("--boot-manifest {boot_manifest}")),
                "the {generation} generation must carry the retained boot manifest: {line}",
            );
            let marker_at = line.find("--fixture-marker").expect("the caller's args are present");
            let injected_at = line.find("--rpc-port").expect("the hub injects the RPC port");
            assert!(marker_at < injected_at, "the caller's args lead the hub's injections: {line}");
        }
        assert_ne!(
            first, second,
            "each fork reserves its own RPC port, so the two command lines cannot be identical: {argv:?}",
        );

        // Tidy up: terminate the successor, which also exercises the
        // group teardown over a shell parent with a real grandchild.
        let terminate = drive(&mailer, &TerminateEngine { engine_id: successor }, Duration::from_secs(30), || {
            cells.terminate.lock().expect("test setup: terminate cell mutex is never poisoned").take()
        });
        assert!(matches!(terminate, TerminateEngineResult::Ok), "the successor terminates cleanly: {terminate:?}");
        reaper.disarm();

        let _ = fs::remove_dir_all(&dir);
    }

    /// A crash loop stops at the burst limit, leaving a final death on
    /// record and no live engine.
    ///
    /// Every generation of this stand-in dies, so an unbounded cap would
    /// re-fork forever — burning ports, scratch dirs and processes for as
    /// long as the hub runs. The limit is what makes automatic restart
    /// safe to turn on, and only a real loop can show that it holds:
    /// `admit_restart` returning `false` in a unit test proves the
    /// arithmetic, not that the cap stops forking. After the budget is
    /// spent the death count must stay put across a window in which a
    /// further restart would comfortably have completed.
    #[test]
    fn a_crash_loop_stops_at_the_burst_limit_and_records_a_final_death() {
        let real = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-engcap-crashloop-{}-{nanos}", process::id()));
        fs::create_dir_all(&dir).expect("test setup: crash-loop temp dir");

        let argv_log = dir.join("argv.log");
        let stand_in = dir.join("aether-headless");
        // No once-marker: every generation crashes.
        write_crashing_stand_in(&stand_in, &real, &argv_log, None);

        // Two restarts, so the loop is the original generation plus two
        // recoveries and then a stop — enough to prove the budget binds
        // without paying for a long loop.
        let burst_limit = 2_u32;
        let expected_deaths = burst_limit as usize + 1;
        let (_registry, _chassis, mailer, cells) =
            boot(restart_config(&dir.join("store"), &dir.join("engines"), &stand_in, burst_limit));

        let spawn = drive(
            &mailer,
            &SpawnEngine { selector: default_selector(), args: vec![], boot_manifest: None },
            Duration::from_secs(FIRST_SPAWN_DEADLINE_SECS),
            || cells.spawn.lock().expect("test setup: spawn cell mutex is never poisoned").take(),
        );
        assert!(
            matches!(spawn, SpawnEngineResult::Ok { .. }),
            "the first generation must come up before it crashes, got {spawn:?}",
        );

        // Run the loop out: the original death plus one per admitted restart.
        let crashes = |list: &ListEnginesResult| {
            list.recently_died.iter().filter(|d| matches!(d.reason, DeathReason::Crashed { .. })).count()
        };
        let reached = poll_list(&mailer, &cells, Duration::from_secs(CRASH_LOOP_DEADLINE_SECS), |list| {
            (crashes(list) >= expected_deaths).then(|| crashes(list))
        })
        .unwrap_or_else(|| {
            panic!(
                "the loop must spend its whole budget ({expected_deaths} crashes) within {CRASH_LOOP_DEADLINE_SECS}s"
            )
        });
        assert_eq!(reached, expected_deaths, "the budget admits exactly {burst_limit} restarts");

        // And then stops. A cap that kept restarting would add another
        // crash inside this window; one that stopped adds nothing.
        thread::sleep(Duration::from_secs(LOOP_SETTLE_SECS));
        let settled = drive(&mailer, &ListEngines {}, Duration::from_secs(15), || {
            cells.list.lock().expect("test setup: list cell mutex is never poisoned").take()
        });
        assert_eq!(
            crashes(&settled),
            expected_deaths,
            "past the burst limit the cap stops restarting; it forked again instead: {settled:?}",
        );
        assert!(settled.engines.is_empty(), "the abandoned lineage leaves no live engine: {settled:?}");

        // The last death is a real recorded crash, not a silent give-up.
        assert!(
            settled.recently_died.iter().any(|d| matches!(d.reason, DeathReason::Crashed { .. })),
            "the final death is recorded normally: {settled:?}",
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

/// Operator-explicit artifact pin coverage (ADR-0115). Independent of
/// restart supervision: these drive `UploadBinary` / `UploadComponent` /
/// `SetArtifactPinned` on an isolated store. Only tests that write a
/// shell pressure stand-in are Unix-gated.
mod operator_pins {
    use super::*;

    #[test]
    fn unnamed_pin_true_headless_survives_tiny_budget_reopen_and_false_reupload() {
        let headless = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-pin-bin-{}-{nanos}", process::id()));
        let engine_root = env::temp_dir().join(format!("aether-engcap-pin-eng-{}-{nanos}", process::id()));
        let config = pin_store_config(&store_dir, &engine_root, 1);

        let hash = {
            let (_registry, chassis, mailer, cells) = boot(config.clone());
            let uploaded = drive(
                &mailer,
                &UploadBinary { staged_path: headless.clone(), name: None, pin: true },
                Duration::from_secs(30),
                || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
            );
            let hash = match uploaded {
                UploadBinaryResult::Ok { hash, name } => {
                    assert!(name.is_none(), "unnamed pin:true must not invent a name");
                    hash
                }
                UploadBinaryResult::Err { error } => panic!("pin:true headless upload failed: {error}"),
            };
            let listed = drive(&mailer, &history_binaries(), Duration::from_secs(5), || {
                cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned").take()
            });
            assert!(
                listed.binaries.iter().any(|entry| entry.hash == hash),
                "list(include_history) must find the unnamed pinned hash: {listed:?}"
            );

            let again = drive(
                &mailer,
                &UploadBinary { staged_path: headless, name: None, pin: false },
                Duration::from_secs(30),
                || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
            );
            match again {
                UploadBinaryResult::Ok { hash: again_hash, .. } => assert_eq!(again_hash, hash),
                UploadBinaryResult::Err { error } => panic!("pin:false reupload failed: {error}"),
            }
            drop(chassis);
            hash
        };

        {
            let (_registry, chassis, mailer, cells) = boot(config);
            let listed = drive(&mailer, &history_binaries(), Duration::from_secs(5), || {
                cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned").take()
            });
            assert!(
                listed.binaries.iter().any(|entry| entry.hash == hash),
                "the durable pin must survive cap reopen under the tiny budget: {listed:?}"
            );
            drop(chassis);
        }
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&engine_root);
    }

    #[test]
    fn unnamed_pin_true_component_survives_tiny_budget() {
        if !aether_harness_fleet::dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let wasm = aether_harness_fleet::component_wasm_path("aether_test_fixtures_bundle");
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-pin-wasm-{}-{nanos}", process::id()));
        let engine_root = env::temp_dir().join(format!("aether-engcap-pin-wasm-eng-{}-{nanos}", process::id()));
        let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 1));
        let uploaded = drive(
            &mailer,
            &UploadComponent { staged_path: wasm.to_string_lossy().into_owned(), name: None, pin: true },
            Duration::from_secs(15),
            || cells.upload_component.lock().expect("test setup: upload_component cell mutex is never poisoned").take(),
        );
        let hash = match uploaded {
            UploadComponentResult::Ok { hash, name } => {
                assert!(name.is_none());
                hash
            }
            UploadComponentResult::Err { error } => panic!("pin:true component upload failed: {error}"),
        };
        let listed = drive(&mailer, &history_components(), Duration::from_secs(5), || {
            cells.list_components.lock().expect("test setup: list_components cell mutex is never poisoned").take()
        });
        assert!(
            listed.components.iter().any(|entry| entry.hash == hash),
            "list(include_history) must find the unnamed pinned component: {listed:?}"
        );
        drop(chassis);
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&engine_root);
    }

    #[test]
    fn dedup_pin_true_headless_upgrades_under_tiny_budget_on_reopen() {
        let headless = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-pin-dedup-{}-{nanos}", process::id()));
        let engine_root = env::temp_dir().join(format!("aether-engcap-pin-dedup-eng-{}-{nanos}", process::id()));

        let hash = {
            let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 17_179_869_184));
            let uploaded = drive(
                &mailer,
                &UploadBinary { staged_path: headless.clone(), name: None, pin: false },
                Duration::from_secs(30),
                || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
            );
            let hash = match uploaded {
                UploadBinaryResult::Ok { hash, .. } => hash,
                UploadBinaryResult::Err { error } => panic!("seed upload failed: {error}"),
            };
            drop(chassis);
            hash
        };

        let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 1));
        let listed = drive(&mailer, &history_binaries(), Duration::from_secs(5), || {
            cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned").take()
        });
        assert!(
            listed.binaries.iter().any(|entry| entry.hash == hash),
            "open does not evict; the unpinned seed is still listed under a tiny budget: {listed:?}"
        );
        let upgraded = drive(
            &mailer,
            &UploadBinary { staged_path: headless, name: None, pin: true },
            Duration::from_secs(30),
            || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
        );
        match upgraded {
            UploadBinaryResult::Ok { hash: again, .. } => assert_eq!(again, hash),
            UploadBinaryResult::Err { error } => panic!("dedup pin:true failed: {error}"),
        }
        let listed = drive(&mailer, &history_binaries(), Duration::from_secs(5), || {
            cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned").take()
        });
        assert!(
            listed.binaries.iter().any(|entry| entry.hash == hash),
            "dedup pin:true must persist before that upload's eviction: {listed:?}"
        );
        drop(chassis);
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&engine_root);
    }

    #[test]
    fn set_artifact_pinned_unknown_hash_does_not_resolve_names() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-pin-unknown-{}-{nanos}", process::id()));
        let engine_root = env::temp_dir().join(format!("aether-engcap-pin-unknown-eng-{}-{nanos}", process::id()));
        let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 1));
        let by_name = drive(
            &mailer,
            &SetArtifactPinned { hash: "keep".to_owned(), pinned: true },
            Duration::from_secs(5),
            || cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take(),
        );
        match by_name {
            SetArtifactPinnedResult::Err { error } => {
                assert!(error.contains("no stored artifact has hash"), "must not resolve a name: {error}");
            }
            SetArtifactPinnedResult::Ok { .. } => panic!("SetArtifactPinned must not resolve names"),
        }
        let unknown =
            drive(&mailer, &SetArtifactPinned { hash: "0".repeat(64), pinned: true }, Duration::from_secs(5), || {
                cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take()
            });
        assert!(
            matches!(unknown, SetArtifactPinnedResult::Err { ref error } if error.contains("no stored artifact has hash")),
            "unknown content hash must error: {unknown:?}"
        );
        drop(chassis);
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&engine_root);
    }

    #[cfg(unix)]
    #[test]
    fn set_artifact_pinned_mail_does_not_resolve_names_and_preserves_name_protection() {
        let headless = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-pin-mail-{}-{nanos}", process::id()));
        let engine_root = env::temp_dir().join(format!("aether-engcap-pin-mail-eng-{}-{nanos}", process::id()));
        let pressure = env::temp_dir().join(format!("aether-engcap-pin-mail-pressure-{}-{nanos}", process::id()));
        let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 1));

        let uploaded = drive(
            &mailer,
            &UploadBinary { staged_path: headless, name: Some("keep".to_owned()), pin: false },
            Duration::from_secs(30),
            || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
        );
        let hash = match uploaded {
            UploadBinaryResult::Ok { hash, name } => {
                assert_eq!(name.as_deref(), Some("keep"));
                hash
            }
            UploadBinaryResult::Err { error } => panic!("named upload failed: {error}"),
        };

        let by_name = drive(
            &mailer,
            &SetArtifactPinned { hash: "keep".to_owned(), pinned: true },
            Duration::from_secs(5),
            || cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take(),
        );
        match by_name {
            SetArtifactPinnedResult::Err { error } => {
                assert!(error.contains("no stored artifact has hash"), "unknown hash must not resolve a name: {error}");
                assert!(!error.contains('/'), "pin errors must stay path-free: {error}");
            }
            SetArtifactPinnedResult::Ok { .. } => panic!("SetArtifactPinned must not resolve names"),
        }

        let unknown =
            drive(&mailer, &SetArtifactPinned { hash: "0".repeat(64), pinned: true }, Duration::from_secs(5), || {
                cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take()
            });
        assert!(
            matches!(unknown, SetArtifactPinnedResult::Err { ref error } if error.contains("no stored artifact has hash")),
            "unknown content hash must error: {unknown:?}"
        );

        let pinned =
            drive(&mailer, &SetArtifactPinned { hash: hash.clone(), pinned: true }, Duration::from_secs(5), || {
                cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take()
            });
        match pinned {
            SetArtifactPinnedResult::Ok { hash: replied, pinned: true } => assert_eq!(replied, hash),
            other => panic!("exact hash pin must succeed: {other:?}"),
        }

        let unpinned =
            drive(&mailer, &SetArtifactPinned { hash: hash.clone(), pinned: false }, Duration::from_secs(5), || {
                cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take()
            });
        match unpinned {
            SetArtifactPinnedResult::Ok { hash: replied, pinned: false } => assert_eq!(replied, hash),
            other => panic!("exact hash unpin must succeed: {other:?}"),
        }

        write_pressure_bin(&pressure, "named-pressure");
        let pressure_upload = drive(
            &mailer,
            &UploadBinary { staged_path: pressure.to_string_lossy().into_owned(), name: None, pin: false },
            Duration::from_secs(15),
            || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
        );
        assert!(
            matches!(pressure_upload, UploadBinaryResult::Ok { .. }),
            "pressure upload must land: {pressure_upload:?}"
        );

        let live = drive(
            &mailer,
            &ListEngineBinaries { chassis: None, caps: Vec::new(), target: None, limit: None, include_history: false },
            Duration::from_secs(5),
            || cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned").take(),
        );
        assert!(
            live.binaries.iter().any(|entry| entry.hash == hash && entry.name.as_deref() == Some("keep")),
            "after unpin and eviction pressure a name still protects: {live:?}"
        );
        drop(chassis);
        let _ = fs::remove_file(&pressure);
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&engine_root);
    }

    #[cfg(unix)]
    #[test]
    fn set_artifact_pinned_unnamed_pin_survives_pressure_and_unpin_is_reclaimable() {
        let headless = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-pin-unnamed-{}-{nanos}", process::id()));
        let engine_root = env::temp_dir().join(format!("aether-engcap-pin-unnamed-eng-{}-{nanos}", process::id()));
        let pressure_keep = env::temp_dir().join(format!("aether-engcap-pin-unnamed-keep-{}-{nanos}", process::id()));
        let pressure_reap = env::temp_dir().join(format!("aether-engcap-pin-unnamed-reap-{}-{nanos}", process::id()));

        let hash = {
            let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 17_179_869_184));
            let uploaded = drive(
                &mailer,
                &UploadBinary { staged_path: headless, name: None, pin: false },
                Duration::from_secs(30),
                || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
            );
            let hash = match uploaded {
                UploadBinaryResult::Ok { hash, name } => {
                    assert!(name.is_none());
                    hash
                }
                UploadBinaryResult::Err { error } => panic!("unnamed seed upload failed: {error}"),
            };
            let pinned =
                drive(&mailer, &SetArtifactPinned { hash: hash.clone(), pinned: true }, Duration::from_secs(5), || {
                    cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take()
                });
            match pinned {
                SetArtifactPinnedResult::Ok { hash: replied, pinned: true } => assert_eq!(replied, hash),
                other => panic!("unnamed pin via mail must succeed: {other:?}"),
            }
            drop(chassis);
            hash
        };

        let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 1));
        write_pressure_bin(&pressure_keep, "keep-pressure");
        let keep_upload = drive(
            &mailer,
            &UploadBinary { staged_path: pressure_keep.to_string_lossy().into_owned(), name: None, pin: false },
            Duration::from_secs(15),
            || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
        );
        assert!(matches!(keep_upload, UploadBinaryResult::Ok { .. }), "keep-pressure upload: {keep_upload:?}");
        let listed = drive(&mailer, &history_binaries(), Duration::from_secs(5), || {
            cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned").take()
        });
        assert!(
            listed.binaries.iter().any(|entry| entry.hash == hash),
            "explicit pin via mail must retain the unnamed hash under tiny-budget pressure: {listed:?}"
        );

        let unpinned =
            drive(&mailer, &SetArtifactPinned { hash: hash.clone(), pinned: false }, Duration::from_secs(5), || {
                cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take()
            });
        match unpinned {
            SetArtifactPinnedResult::Ok { hash: replied, pinned: false } => assert_eq!(replied, hash),
            other => panic!("unnamed unpin via mail must succeed: {other:?}"),
        }

        write_pressure_bin(&pressure_reap, "reap-pressure");
        let reap_upload = drive(
            &mailer,
            &UploadBinary { staged_path: pressure_reap.to_string_lossy().into_owned(), name: None, pin: false },
            Duration::from_secs(15),
            || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
        );
        assert!(matches!(reap_upload, UploadBinaryResult::Ok { .. }), "reap-pressure upload: {reap_upload:?}");
        let listed = drive(&mailer, &history_binaries(), Duration::from_secs(5), || {
            cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned").take()
        });
        assert!(
            listed.binaries.iter().all(|entry| entry.hash != hash),
            "after unpin, pressure must reclaim the unnamed hash: {listed:?}"
        );
        drop(chassis);
        let _ = fs::remove_file(&pressure_keep);
        let _ = fs::remove_file(&pressure_reap);
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&engine_root);
    }

    #[cfg(unix)]
    #[test]
    fn set_artifact_pinned_unpin_persistence_failure_keeps_in_memory_protection() {
        let headless = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-pin-fail-{}-{nanos}", process::id()));
        let engine_root = env::temp_dir().join(format!("aether-engcap-pin-fail-eng-{}-{nanos}", process::id()));
        let pressure = env::temp_dir().join(format!("aether-engcap-pin-fail-pressure-{}-{nanos}", process::id()));
        let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 1));

        let uploaded = drive(
            &mailer,
            &UploadBinary { staged_path: headless, name: None, pin: true },
            Duration::from_secs(30),
            || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
        );
        let hash = match uploaded {
            UploadBinaryResult::Ok { hash, .. } => hash,
            UploadBinaryResult::Err { error } => panic!("pinned seed upload failed: {error}"),
        };
        occupy_owned_sidecar_as_dir(&store_dir, &hash);

        let failed =
            drive(&mailer, &SetArtifactPinned { hash: hash.clone(), pinned: false }, Duration::from_secs(5), || {
                cells.set_pinned.lock().expect("test setup: set_pinned cell mutex is never poisoned").take()
            });
        match failed {
            SetArtifactPinnedResult::Err { error } => {
                assert_path_free_persist_error(&error, "unpinning artifact", &store_dir);
            }
            SetArtifactPinnedResult::Ok { .. } => panic!("failed unpin must not claim success: {failed:?}"),
        }

        write_pressure_bin(&pressure, "fail-pressure");
        let pressure_upload = drive(
            &mailer,
            &UploadBinary { staged_path: pressure.to_string_lossy().into_owned(), name: None, pin: false },
            Duration::from_secs(15),
            || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
        );
        assert!(
            matches!(pressure_upload, UploadBinaryResult::Ok { .. }),
            "pressure upload after failed unpin: {pressure_upload:?}"
        );
        let listed = drive(&mailer, &history_binaries(), Duration::from_secs(5), || {
            cells.list_binaries.lock().expect("test setup: list_binaries cell mutex is never poisoned").take()
        });
        assert!(
            listed.binaries.iter().any(|entry| entry.hash == hash),
            "failed unpin must keep in-memory protection under pressure: {listed:?}"
        );
        drop(chassis);
        let _ = fs::remove_file(&pressure);
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&engine_root);
    }

    #[test]
    fn upload_pin_true_persistence_failure_is_typed_and_path_free() {
        let headless = aether_harness_fleet::headless_bin_path().to_string_lossy().into_owned();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let store_dir = env::temp_dir().join(format!("aether-engcap-pin-upfail-{}-{nanos}", process::id()));
        let engine_root = env::temp_dir().join(format!("aether-engcap-pin-upfail-eng-{}-{nanos}", process::id()));

        let hash = {
            let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 17_179_869_184));
            let uploaded = drive(
                &mailer,
                &UploadBinary { staged_path: headless.clone(), name: None, pin: false },
                Duration::from_secs(30),
                || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
            );
            let hash = match uploaded {
                UploadBinaryResult::Ok { hash, .. } => hash,
                UploadBinaryResult::Err { error } => panic!("seed upload failed: {error}"),
            };
            drop(chassis);
            hash
        };

        occupy_owned_sidecar_as_dir(&store_dir, &hash);
        let (_registry, chassis, mailer, cells) = boot(pin_store_config(&store_dir, &engine_root, 17_179_869_184));
        let failed = drive(
            &mailer,
            &UploadBinary { staged_path: headless, name: None, pin: true },
            Duration::from_secs(30),
            || cells.upload_binary.lock().expect("test setup: upload_binary cell mutex is never poisoned").take(),
        );
        match failed {
            UploadBinaryResult::Err { error } => {
                assert_path_free_persist_error(&error, "pinning uploaded binary", &store_dir);
            }
            UploadBinaryResult::Ok { .. } => panic!("pin:true persist failure must not return a hash: {failed:?}"),
        }
        drop(chassis);
        let _ = fs::remove_dir_all(&store_dir);
        let _ = fs::remove_dir_all(&engine_root);
    }
}
