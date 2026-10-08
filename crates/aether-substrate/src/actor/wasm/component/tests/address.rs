//! The `resolve_path_p32` host fn (ADR-0230 §3, #6786): a WAT guest whose
//! `ask` export forwards straight to the import, over a registry holding a
//! `Live` route and a `Starting` one. Each delivered answer is decoded as the
//! `__ResolvedPath` the SDK decodes.
//!
//! Beside it, the `live_route_p32` host fn (ADR-0230 §3, #7205) and the
//! `route_rows_p32` host fn (ADR-0231 §3, #7501), each driven by the same
//! forwarding guest over its own import and decoded as the `__LiveRoute` or
//! `__PublishedRows` the SDK decodes.

use std::sync::Arc;

use aether_actor::{__LiveRoute, __PublishedRows, __ResolvedPath};
use aether_data::{ReplyContract, wire};
use aether_kinds::{ComponentCapabilities, HandlerCapability};
use serde::de::DeserializeOwned;
use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

use super::{WAT_REALLOC, ctx_at};
use crate::actor::wasm::component::ComponentCtx;
use crate::actor::wasm::host_fns;
use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry, RegistryOwnerLease, RouteContract};
use crate::mail::{KindId, MailboxId};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{Pool, PoolConfig};
use crate::testing::canonical_id;
use crate::testing::{bare_substrate, boot_authority, registered_ref};

/// Where the guest's `ask` export reads the path text from.
const PATH_AT: u32 = 16;

/// A WAT guest whose `ask` export forwards a path's `(ptr, len)` straight to
/// one path-taking import, so a test reads the host fn's answer with no SDK
/// between.
struct PathGuest {
    store: Store<ComponentCtx>,
    memory: Memory,
    ask: TypedFunc<(u32, u32), i64>,
}

impl PathGuest {
    /// Instantiate the guest over `import`, one of the `aether` module's
    /// `(path_ptr, path_len) -> packed` host fns.
    fn over(import: &str, registry: &Arc<Registry>, mailer: Arc<Mailer>) -> Self {
        let engine = Engine::default();
        let mut linker = Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host fns");
        let wat = format!(
            r#"
            (module
                (import "aether" "{import}" (func $ask (param i32 i32) (result i64)))
                (memory (export "memory") 1)
                {WAT_REALLOC}
                (func (export "ask") (param i32 i32) (result i64)
                    local.get 0
                    local.get 1
                    call $ask))
            "#
        );
        let module = Module::new(&engine, wat::parse_str(&wat).expect("compile WAT")).expect("compile module");
        let mut store =
            Store::new(&engine, ctx_at(Arc::clone(registry), mailer, HubOutbound::disconnected(), MailboxId(0), None));
        let instance = linker.instantiate(&mut store, &module).expect("instantiate");
        let memory = instance.get_memory(&mut store, "memory").expect("memory export");
        let ask = instance.get_typed_func::<(u32, u32), i64>(&mut store, "ask").expect("ask export");

        Self { store, memory, ask }
    }

    /// The host fn's answer for `path`, decoded as the SDK decodes it.
    fn answer<A: DeserializeOwned>(&mut self, path: &str) -> A {
        self.memory.write(&mut self.store, PATH_AT as usize, path.as_bytes()).expect("write the path");
        let length = u32::try_from(path.len()).expect("the path fits the 32-bit ABI");
        let packed = u64::from_ne_bytes(
            self.ask.call(&mut self.store, (PATH_AT, length)).expect("the host fn does not trap").to_ne_bytes(),
        );
        let ptr = usize::try_from(packed >> 32).expect("a guest pointer fits usize");
        let len = usize::try_from(packed & u64::from(u32::MAX)).expect("a guest length fits usize");

        wire::from_bytes(&self.memory.data(&self.store)[ptr..][..len]).expect("the answer decodes")
    }
}

/// Catches a refusal arm that crosses the ABI as the wrong variant, and a
/// `Starting` route minted as live.
#[test]
fn resolve_path_answers_live_not_live_and_unresolved_across_the_abi() {
    let (registry, mailer) = bare_substrate();
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, aborter);
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        pool.wake_sink(),
        RegistryQueueCapacities::default(),
    );
    let discharging: Arc<dyn InboxHandler> = Arc::new(|dispatch: OwnedDispatch| dispatch.discharge());
    let live = registered_ref(&registry, "test.wasm.resolve_path_live", discharging);
    let starting = "test.wasm.resolve_path_starting";
    registry.reserve_starting_through_owner(starting).expect("owner accepts the Starting reservation");

    let mut guest = PathGuest::over("resolve_path_p32", &registry, mailer);
    let mut answer = |path: &str| guest.answer::<__ResolvedPath>(path);

    assert_eq!(answer("test.wasm.resolve_path_live"), __ResolvedPath::Live { position: live.id().0 });
    assert_eq!(
        answer(starting),
        __ResolvedPath::NotLive { canonical_path: starting.to_owned() },
        "a Starting route resolves as an address but does not prove",
    );
    assert!(
        matches!(answer("test.wasm.resolve_path_unknown"), __ResolvedPath::Unresolved { .. }),
        "a path with no route is the registry's own refusal",
    );

    drop(guest);
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

/// Catches a `live_route_p32` that answers a `Starting` or never-registered
/// path as live — it must answer a position only for the route standing
/// `Live` under exactly the canonical path.
#[test]
fn live_route_answers_only_a_live_route_under_its_canonical_name() {
    let (registry, mailer) = bare_substrate();
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, aborter);
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        pool.wake_sink(),
        RegistryQueueCapacities::default(),
    );
    let discharging: Arc<dyn InboxHandler> = Arc::new(|dispatch: OwnedDispatch| dispatch.discharge());
    let live = registered_ref(&registry, "test.wasm.live_route_live", discharging);
    let starting = "test.wasm.live_route_starting";
    registry.reserve_starting_through_owner(starting).expect("owner accepts the Starting reservation");

    let mut guest = PathGuest::over("live_route_p32", &registry, mailer);
    let mut answer = |path: &str| guest.answer::<__LiveRoute>(path);

    assert_eq!(answer("test.wasm.live_route_live"), __LiveRoute { position: Some(live.id().0) });
    assert_eq!(answer(starting), __LiveRoute { position: None }, "a Starting route does not prove");
    assert_eq!(
        answer("test.wasm.live_route_unknown"),
        __LiveRoute { position: None },
        "a path with no route answers no position",
    );

    drop(guest);
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

/// A contract publishing exactly `rows`.
fn contract(rows: &[(KindId, ReplyContract)]) -> RouteContract {
    RouteContract::from_capabilities(&ComponentCapabilities {
        handlers: rows
            .iter()
            .map(|(id, reply)| HandlerCapability {
                id: *id,
                name: String::new(),
                doc: None,
                reply: *reply,
                reason: None,
            })
            .collect(),
        ..ComponentCapabilities::default()
    })
}

/// Catches a `route_rows_p32` backed by a `Live`-only read, which would make
/// a closed actor's path refuse in a guest and prove natively; one that
/// answers a `Starting` or never-registered path with rows a guest would
/// prove against; and one that skips the canonical-name check and answers
/// the rows of another route standing at the path's fold.
#[test]
fn route_rows_answers_a_live_or_dropped_route_under_its_canonical_name() {
    let (registry, mailer) = bare_substrate();
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, aborter);
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        pool.wake_sink(),
        RegistryQueueCapacities::default(),
    );
    let discharging = || -> Arc<dyn InboxHandler> { Arc::new(|dispatch: OwnedDispatch| dispatch.discharge()) };
    let rows = vec![(KindId(1), ReplyContract::None), (KindId(2), ReplyContract::One(KindId(10)))];

    let live = "test.wasm.route_rows_live";
    let live_id = registered_ref(&registry, live, discharging()).id();
    registry.publish_contract_through_owner(live_id, contract(&rows)).expect("the live route publishes its rows");

    let dropped = "test.wasm.route_rows_dropped";
    let dropped_id = registered_ref(&registry, dropped, discharging()).id();
    registry.publish_contract_through_owner(dropped_id, contract(&rows)).expect("the route publishes its rows");
    registry.drop_mailbox(&boot_authority(), dropped_id).expect("the live route retires");

    let starting = "test.wasm.route_rows_starting";
    registry.reserve_starting_through_owner(starting).expect("owner accepts the Starting reservation");

    let folded = "test.wasm.route_rows_folded";
    let impostor = registry
        .try_register_inbox_with_id(
            &boot_authority(),
            canonical_id(folded),
            "test.wasm.route_rows_impostor",
            discharging(),
        )
        .expect("the fold is free");
    registry.publish_contract_through_owner(impostor, contract(&rows)).expect("the impostor publishes its rows");

    let mut guest = PathGuest::over("route_rows_p32", &registry, mailer);
    let mut answer = |path: &str| guest.answer::<__PublishedRows>(path);

    assert_eq!(answer(live), __PublishedRows { rows: Some(rows.clone()) });
    assert_eq!(
        answer(dropped),
        __PublishedRows { rows: Some(rows) },
        "a closed actor's path still proves its type; `resolve` answers that it is not live",
    );
    for path in [starting, "test.wasm.route_rows_never", folded] {
        assert_eq!(answer(path), __PublishedRows { rows: None }, "{path}");
    }

    drop(guest);
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}
