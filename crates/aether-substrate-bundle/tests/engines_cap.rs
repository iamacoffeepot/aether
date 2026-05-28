// End-to-end test for the engines cap (issue 763 P4).
//
// Boots a passive chassis hosting `EngineServer`, mails it a
// `SpawnEngine` pointed at the real `aether-substrate-headless`
// binary, and asserts the full lifecycle: the substrate forks and
// binds its RPC port, the per-engine proxy bridges the startup gap
// and connects, `ListEngines` reflects the live engine, and
// `TerminateEngine` shuts it down. This is the only test exercising
// the fork+exec + startup-race-retry + real-process path — the
// `EngineServer` unit tests cover the error arms in-process.

use aether_actor::Actor;
use aether_capabilities::EngineServer;
use aether_data::{Kind, mailbox_id_from_name};
use aether_kinds::descriptors;
use aether_kinds::{
    ListEngines, ListEnginesResult, SpawnEngine, SpawnEngineResult, TerminateEngine,
    TerminateEngineResult,
};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::handle_store::HandleStore;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use aether_substrate::mail::{Mail, ReplyTarget, ReplyTo};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TestChassis;
impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is driven by Builder::new directly in this test")
    }
}

// Reply sink: records the latest reply of each engines-cap reply kind
// into shared cells. Lives at module root so the `#[bridge]` macro's
// marker emission stays addressable.
#[derive(Clone, Default)]
pub struct ReplyCells {
    pub list: Arc<Mutex<Option<ListEnginesResult>>>,
    pub spawn: Arc<Mutex<Option<SpawnEngineResult>>>,
    pub terminate: Arc<Mutex<Option<TerminateEngineResult>>>,
}

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
                .expect("test setup: list cell mutex is never poisoned") = Some(reply);
        }

        #[handler]
        fn on_spawn_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: SpawnEngineResult) {
            *self
                .cells
                .spawn
                .lock()
                .expect("test setup: spawn cell mutex is never poisoned") = Some(reply);
        }

        #[handler]
        fn on_terminate_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: TerminateEngineResult) {
            *self
                .cells
                .terminate
                .lock()
                .expect("test setup: terminate cell mutex is never poisoned") = Some(reply);
        }
    }
}

// `ReplySink` is re-exported to this module root by the `#[bridge]`
// macro — no explicit `use sink::ReplySink` needed.

fn boot() -> (PassiveChassis<TestChassis>, Arc<Mailer>, ReplyCells) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let store = Arc::new(HandleStore::new(1024 * 1024));
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
    let cells = ReplyCells::default();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EngineServer>(())
        .with_actor::<ReplySink>(cells.clone())
        .build_passive()
        .expect("caps boot");
    (chassis, mailer, cells)
}

/// Drive one request kind at `aether.engine`, reply-to the sink, and
/// block until `probe` returns a recorded reply (or `deadline` passes).
fn drive<K: Kind + serde::Serialize, T>(
    mailer: &Arc<Mailer>,
    request: &K,
    deadline: Duration,
    probe: impl Fn() -> Option<T>,
) -> T {
    let server = mailbox_id_from_name(<EngineServer as Actor>::NAMESPACE);
    let sink = mailbox_id_from_name(<ReplySink as Actor>::NAMESPACE);
    mailer.push(
        Mail::new(server, K::ID, request.encode_into_bytes(), 1)
            .with_reply_to(ReplyTo::with_correlation(ReplyTarget::Component(sink), 1)),
    );
    let until = Instant::now() + deadline;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < until, "no reply within {deadline:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn engines_cap_spawns_lists_and_terminates_a_real_headless_substrate() {
    let (_chassis, mailer, cells) = boot();
    let headless = env!("CARGO_BIN_EXE_aether-substrate-headless");

    // Spawn: the cap assigns a port, forks the substrate, and the
    // proxy retries the dial until the fresh process binds. Generous
    // deadline — this covers a debug-build chassis cold start.
    let spawn = drive(
        &mailer,
        &SpawnEngine {
            binary_path: headless.to_owned(),
            args: vec![],
        },
        Duration::from_secs(30),
        || {
            cells
                .spawn
                .lock()
                .expect("test setup: spawn cell mutex is never poisoned")
                .take()
        },
    );
    let engine_id = match spawn {
        SpawnEngineResult::Ok {
            engine_id,
            rpc_port,
        } => {
            assert_ne!(rpc_port, 0, "cap should report the assigned RPC port");
            engine_id
        }
        SpawnEngineResult::Err { error } => panic!("spawn failed: {error}"),
    };

    // List: the freshly-spawned engine shows up in the cap's table.
    let list = drive(&mailer, &ListEngines {}, Duration::from_secs(5), || {
        cells
            .list
            .lock()
            .expect("test setup: list cell mutex is never poisoned")
            .take()
    });
    assert!(
        list.engines.iter().any(|e| e.engine_id == engine_id),
        "spawned engine {engine_id} should appear in ListEngines: {list:?}",
    );

    // Terminate: the cap forwards to the proxy, which SIGKILLs the
    // substrate and self-shuts-down; the table entry is dropped.
    let terminate = drive(
        &mailer,
        &TerminateEngine {
            engine_id: engine_id.clone(),
        },
        Duration::from_secs(5),
        || {
            cells
                .terminate
                .lock()
                .expect("test setup: terminate cell mutex is never poisoned")
                .take()
        },
    );
    assert!(
        matches!(terminate, TerminateEngineResult::Ok),
        "terminate of a live engine should succeed: {terminate:?}",
    );

    // After terminate, the engine is gone from the table.
    let list_after = drive(&mailer, &ListEngines {}, Duration::from_secs(5), || {
        cells
            .list
            .lock()
            .expect("test setup: list cell mutex is never poisoned")
            .take()
    });
    assert!(
        !list_after.engines.iter().any(|e| e.engine_id == engine_id),
        "terminated engine {engine_id} should be gone from ListEngines: {list_after:?}",
    );
}

/// Two engines spawned via the cap coexist with persistence on: each
/// gets its own `${AETHER_ENGINE_STORE_ROOT}/<engine_id>` handle-store
/// dir, so the ADR-0049 §7 `lock.pid` doesn't collide
/// (iamacoffeepot/aether#1274). Before the fix, both substrates
/// resolved to the same default `dirs::data_dir()/aether/handles` and
/// one failed with `LockError::Held`.
#[test]
fn two_engines_get_distinct_handle_store_dirs() {
    let headless = env!("CARGO_BIN_EXE_aether-substrate-headless");

    // Per-test scratch dir under the system temp root. Process pid +
    // nanos disambiguate against any leftover from a prior run that
    // didn't clean up (the test below cleans on the success path).
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let root = env::temp_dir().join(format!("aether-engines-cap-{}-{}", process::id(), nanos));

    // SAFETY: nextest runs each test in its own process, so the env
    // mutations here don't race sibling tests. We need (a) the cap's
    // `engine_store_root()` to resolve to `root` (read in the test
    // process when `on_spawn` runs), (b) the spawned substrates to
    // NOT inherit a parent `AETHER_HANDLE_STORE_DIR` (which would win
    // over the cap's per-engine injection), and (c) persistence not
    // to be globally disabled — the lock-collision case the issue
    // pins only fires when each substrate writes a `lock.pid`.
    unsafe {
        env::set_var("AETHER_ENGINE_STORE_ROOT", &root);
        env::remove_var("AETHER_HANDLE_STORE_DIR");
        env::remove_var("AETHER_HANDLE_STORE_PERSIST_DISABLE");
    }

    let (_chassis, mailer, cells) = boot();

    // Spawn engine A.
    let a = drive(
        &mailer,
        &SpawnEngine {
            binary_path: headless.to_owned(),
            args: vec![],
        },
        Duration::from_secs(30),
        || {
            cells
                .spawn
                .lock()
                .expect("test setup: spawn cell mutex is never poisoned")
                .take()
        },
    );
    let a_id = match a {
        SpawnEngineResult::Ok { engine_id, .. } => engine_id,
        SpawnEngineResult::Err { error } => panic!("spawn A failed: {error}"),
    };

    // Spawn engine B. Pre-fix this would race the same handle-store
    // dir and either A or B would die with `LockError::Held`; the
    // cap's reply to `on_spawn` would be `Err` because the proxy
    // never connected to a substrate that aborted boot. The assertion
    // below is structural — both spawn replies are Ok.
    let b = drive(
        &mailer,
        &SpawnEngine {
            binary_path: headless.to_owned(),
            args: vec![],
        },
        Duration::from_secs(30),
        || {
            cells
                .spawn
                .lock()
                .expect("test setup: spawn cell mutex is never poisoned")
                .take()
        },
    );
    let b_id = match b {
        SpawnEngineResult::Ok { engine_id, .. } => engine_id,
        SpawnEngineResult::Err { error } => panic!("spawn B failed: {error}"),
    };
    assert_ne!(a_id, b_id, "the cap must mint distinct engine ids");

    // Walk `root/` for the per-engine subdirs the cap allocated.
    // Each child substrate creates `lock.pid` + `v1/` lazily under
    // its assigned `AETHER_HANDLE_STORE_DIR`, so we only assert on the
    // top-level subdir count — the lock collision (the actual bug)
    // would surface as a missing dir or a failed spawn earlier.
    let subdirs: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read_dir({}): {e}", root.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        subdirs.len(),
        2,
        "expected two per-engine handle-store dirs under {}; saw: {subdirs:?}",
        root.display(),
    );

    // Terminate both engines so their proxies SIGKILL the substrates;
    // best-effort cleanup of the scratch root follows.
    for id in [a_id, b_id] {
        let _ = drive(
            &mailer,
            &TerminateEngine { engine_id: id },
            Duration::from_secs(5),
            || {
                cells
                    .terminate
                    .lock()
                    .expect("test setup: terminate cell mutex is never poisoned")
                    .take()
            },
        );
    }
    let _ = fs::remove_dir_all(&root);
}
