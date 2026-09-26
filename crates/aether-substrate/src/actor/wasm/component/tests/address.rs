//! The `resolve_path_p32` host fn (ADR-0230 §3, #6786): a WAT guest whose
//! `resolve` export forwards straight to the import, over a registry holding a
//! `Live` route and a `Starting` one. Each delivered answer is decoded as the
//! `__ResolvedPath` the SDK decodes.

use std::sync::Arc;

use aether_actor::__ResolvedPath;
use aether_data::wire;
use wasmtime::{Engine, Linker, Module, Store};

use super::{WAT_REALLOC, ctx_at};
use crate::actor::wasm::host_fns;
use crate::config::RegistryQueueCapacities;
use crate::mail::MailboxId;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{InboxHandler, OwnedDispatch, RegistryOwnerLease};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{Pool, PoolConfig};
use crate::testing::{bare_substrate, boot_authority, registered_ref};

/// Where the guest's `resolve` export reads the path text from.
const PATH_AT: u32 = 16;

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

    let engine = Engine::default();
    let mut linker = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");
    let wat = format!(
        r#"
        (module
            (import "aether" "resolve_path_p32" (func $resolve (param i32 i32) (result i64)))
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "resolve") (param i32 i32) (result i64)
                local.get 0
                local.get 1
                call $resolve))
        "#
    );
    let module = Module::new(&engine, wat::parse_str(&wat).expect("compile WAT")).expect("compile module");
    let mut store =
        Store::new(&engine, ctx_at(Arc::clone(&registry), mailer, HubOutbound::disconnected(), MailboxId(0), None));
    let instance = linker.instantiate(&mut store, &module).expect("instantiate");
    let memory = instance.get_memory(&mut store, "memory").expect("memory export");
    let resolve = instance.get_typed_func::<(u32, u32), i64>(&mut store, "resolve").expect("resolve export");

    let mut answer = |path: &str| -> __ResolvedPath {
        memory.write(&mut store, PATH_AT as usize, path.as_bytes()).expect("write the path");
        let length = u32::try_from(path.len()).expect("the path fits the 32-bit ABI");
        let packed = u64::from_ne_bytes(
            resolve.call(&mut store, (PATH_AT, length)).expect("resolve_path_p32 does not trap").to_ne_bytes(),
        );
        let ptr = usize::try_from(packed >> 32).expect("a guest pointer fits usize");
        let len = usize::try_from(packed & u64::from(u32::MAX)).expect("a guest length fits usize");
        wire::from_bytes(&memory.data(&store)[ptr..][..len]).expect("the answer decodes as __ResolvedPath")
    };

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

    drop(store);
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}
