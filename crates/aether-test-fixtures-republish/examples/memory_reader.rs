//! Issue 7622: a guest that asks the engine what it holds, the way a debug
//! overlay does.
//!
//! - `MemoryReader` (`test.memory.reader`, root) answers each `MemoryQuery`
//!   with a `MemoryReadResult` of the rows in the `ListMemoryResult` the inventory
//!   cap replies to its own `ListMemory`. The report is the fixture's own
//!   kind because a held reply needs a failure answer for a holder that
//!   closes, and `ListMemoryResult` has no failure arm.

use aether_actor::{ActorInitError, Held, Pending, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_inventory::InventoryCapability;
use aether_inventory::kinds::{ListMemory, ListMemoryResult};
use aether_test_fixtures_kinds::{MemoryQuery, MemoryReadResult, MemoryRow};

/// Carried across the inventory send: the reply owed to the requester.
#[aether_data::kind(name = "aether.test_fixtures.memory_reader_context")]
struct MemoryContext {
    held: Held<MemoryReadResult>,
}

/// Holds nothing between requests: each query's state rides its context.
pub struct MemoryReader;

#[actor(root, depends(InventoryCapability))]
impl WasmActor for MemoryReader {
    const NAMESPACE: &'static str = "test.memory.reader";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(MemoryReader)
    }

    #[handler::request]
    fn on_memory_query(&mut self, ctx: &mut WasmCtx<'_>, _query: MemoryQuery) -> Pending<MemoryReadResult> {
        let (pending, held) = ctx.hold::<MemoryReadResult>();
        let _ = ctx.send_with_context::<InventoryCapability>(&ListMemory {}, MemoryContext { held });
        pending
    }

    #[handler::response]
    fn on_memory(&mut self, ctx: &mut WasmCtx<'_>, result: ListMemoryResult, MemoryContext { held }: MemoryContext) {
        let owners = result
            .owners
            .into_iter()
            .map(|row| MemoryRow { owner: row.owner, label: row.label, bytes: row.bytes })
            .collect();
        held.answer(ctx, &MemoryReadResult::Rows { owners });
    }
}

aether_actor::export!(public = [MemoryReader]);
