//! A guest's linear memory on the engine's memory ledger: the row a
//! component's store keeps through wasmtime's resource limiter.

use std::sync::Arc;

use wasmtime::{Engine, Linker, Module};

use super::{ctx_at, inbound};
use crate::actor::wasm::component::{Component, ComponentCtx};
use crate::actor::wasm::host_fns;
use crate::mail::outbound::HubOutbound;
use crate::mail::{MailboxId, Source};
use crate::testing::bare_substrate;

const PAGE_BYTES: u64 = 0x1_0000;

/// One page at instantiate; each mail grows the memory by one page, and a
/// growth past the two-page maximum fails. Its allocator hands out one fixed
/// region inside the first page, so delivery itself never grows the memory.
const WAT_GROWS_ON_RECEIVE: &str = r#"
        (module
            (memory (export "memory") 1 2)
            (func (export "realloc_p32") (param i32 i32 i32 i32) (result i32)
                i32.const 1024)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                (drop (memory.grow (i32.const 1)))
                i32.const 0))
    "#;

/// The row follows the guest's memory: its initial size at instantiate, the
/// grown size after a `memory.grow`, the unchanged size after a growth the
/// memory's maximum refuses, and no row once the component drops. The bugs
/// this catches are a limiter that is never installed, so the row never
/// moves, a refused growth counted as if it happened, and a dead component's
/// row staying in the report.
#[test]
fn a_component_row_follows_its_linear_memory() {
    let (registry, mailer) = bare_substrate();
    let ctx = ctx_at(registry, Arc::clone(&mailer), HubOutbound::disconnected(), MailboxId(0), None);
    let engine = Engine::default();
    let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");
    let module = Module::new(&engine, wat::parse_str(WAT_GROWS_ON_RECEIVE).expect("compile WAT")).expect("module");
    let mut component = Component::instantiate(&engine, &linker, &module, ctx, &[], None).expect("instantiate");
    let rows = || -> Vec<(&'static str, u64)> {
        mailer.memory_report().owners.iter().map(|row| (row.label, row.bytes)).collect()
    };
    let mut receive =
        || component.deliver(&inbound(MailboxId(0), aether_data::KindId(0), vec![], Source::NONE)).expect("deliver");

    assert_eq!(rows(), [("linear memory", PAGE_BYTES)]);

    receive();
    assert_eq!(rows(), [("linear memory", 2 * PAGE_BYTES)]);

    receive();
    assert_eq!(rows(), [("linear memory", 2 * PAGE_BYTES)], "a refused growth is not counted");

    drop(component);
    assert_eq!(rows(), []);
}
