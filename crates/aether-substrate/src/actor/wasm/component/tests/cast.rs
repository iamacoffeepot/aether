//! The `published_rows_p32` host fn (ADR-0231 §4): a WAT guest whose `rows`
//! export forwards straight to the import, over a registry holding a `Live`
//! route that published rows and a `Starting` one. Each delivered answer is
//! decoded as the `__PublishedRows` the SDK's `WasmCtx::cast` decodes.

use std::sync::Arc;

use aether_actor::__PublishedRows;
use aether_data::{ReplyContract, wire};
use aether_kinds::{ComponentCapabilities, HandlerCapability};
use wasmtime::{Engine, Linker, Module, Store};

use super::{WAT_REALLOC, ctx_at};
use crate::actor::wasm::host_fns;
use crate::config::RegistryQueueCapacities;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{InboxHandler, OwnedDispatch, RegistryOwnerLease, RouteContract};
use crate::mail::{KindId, MailboxId};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{Pool, PoolConfig};
use crate::testing::{bare_substrate, boot_authority, registered_ref};

/// Catches rows read from the wrong route, and a `Starting` or unknown route
/// answered with rows a guest would cast against.
#[test]
fn published_rows_answers_a_live_routes_exact_rows_and_none_otherwise() {
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

    let rows = vec![
        (KindId(1), ReplyContract::None),
        (KindId(2), ReplyContract::One(KindId(10))),
        (KindId(3), ReplyContract::Unchecked),
    ];
    let contract = RouteContract::from_capabilities(&ComponentCapabilities {
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
    });
    let discharging: Arc<dyn InboxHandler> = Arc::new(|dispatch: OwnedDispatch| dispatch.discharge());
    let live = registered_ref(&registry, "test.wasm.published_rows_live", discharging).id();
    registry.publish_contract_through_owner(live, contract).expect("the live route publishes its rows");
    let (starting, _token) = registry
        .reserve_starting_through_owner("test.wasm.published_rows_starting")
        .expect("owner accepts the Starting reservation");

    let engine = Engine::default();
    let mut linker = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");
    let wat = format!(
        r#"
        (module
            (import "aether" "published_rows_p32" (func $rows (param i64) (result i64)))
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "rows") (param i64) (result i64)
                local.get 0
                call $rows))
        "#
    );
    let module = Module::new(&engine, wat::parse_str(&wat).expect("compile WAT")).expect("compile module");
    let mut store =
        Store::new(&engine, ctx_at(Arc::clone(&registry), mailer, HubOutbound::disconnected(), MailboxId(0), None));
    let instance = linker.instantiate(&mut store, &module).expect("instantiate");
    let memory = instance.get_memory(&mut store, "memory").expect("memory export");
    let published = instance.get_typed_func::<u64, i64>(&mut store, "rows").expect("rows export");

    let mut answer = |position: MailboxId| -> __PublishedRows {
        let packed = u64::from_ne_bytes(
            published.call(&mut store, position.0).expect("published_rows_p32 does not trap").to_ne_bytes(),
        );
        let ptr = usize::try_from(packed >> 32).expect("a guest pointer fits usize");
        let len = usize::try_from(packed & u64::from(u32::MAX)).expect("a guest length fits usize");
        wire::from_bytes(&memory.data(&store)[ptr..][..len]).expect("the answer decodes as __PublishedRows")
    };

    assert_eq!(answer(live), __PublishedRows { rows: Some(rows) });
    assert_eq!(answer(starting), __PublishedRows { rows: None }, "a Starting route is not castable");
    assert_eq!(answer(MailboxId(0xdead_beef)), __PublishedRows { rows: None }, "an unknown position has no rows");

    drop(store);
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}
