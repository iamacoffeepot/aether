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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    for d in aether_kinds::descriptors::all() {
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
        std::thread::sleep(Duration::from_millis(25));
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
            assert!(rpc_port != 0, "cap should report the assigned RPC port");
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
