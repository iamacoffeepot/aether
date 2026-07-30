// End-to-end test for the engines cap (issue 763 P4).
//
// Boots a passive chassis hosting `FleetServer`, mails it a
// `SpawnEngine` pointed at the real `aether-substrate-headless`
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
    BinarySelector, DeathReason, ListEngines, ListEnginesResult, SpawnEngine, SpawnEngineResult, TerminateEngine,
    TerminateEngineResult,
};
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::{MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::mail::{Mail, MailId, MailRef, Source, SourceAddr};
use aether_substrate::testing::TestChassis;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
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
}

fn boot(engine_config: FleetConfig) -> (Arc<Registry>, PassiveChassis<TestChassis>, Arc<Mailer>, ReplyCells) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
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
        K::NAME.to_owned(),
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

        registry.drop_mailbox(collision).expect("remove test-only proxy collision authority");
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
        let stand_in = dir.join("aether-substrate-headless");
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
}
