//! Issue 7086: a guest that loads components the way any guest does, by
//! mailing `LoadComponent` to the component host, so a test can issue a load
//! from inside the engine while a republish is in flight.
//!
//! - `Loader` (`test.republish.loader`, root) answers each `GuestLoad` with
//!   the `LoadResult` its load receives, holding the reply across the load.

use aether_actor::{ActorInitError, Held, Pending, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_component::ComponentHostCapability;
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::GuestLoad;

/// The reply one relayed load owes its requester.
#[aether_data::kind(name = "aether.test_fixtures.republish_loader_context")]
struct LoaderContext {
    held: Held<LoadResult>,
}

/// Counts the loads it has issued and keeps each answer it relayed.
pub struct Loader {
    issued: u32,
    relayed: Vec<LoadResult>,
}

#[actor(root, depends(ComponentHostCapability))]
impl WasmActor for Loader {
    const NAMESPACE: &'static str = "test.republish.loader";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Loader { issued: 0, relayed: Vec::new() })
    }

    #[handler::request]
    fn on_load(&mut self, ctx: &mut WasmCtx<'_>, request: GuestLoad) -> Pending<LoadResult> {
        let (pending, held) = ctx.hold::<LoadResult>();
        let GuestLoad { wasm, name, export } = request;
        let load = LoadComponent { wasm, name, config: Vec::new(), export };
        let _ = ctx.send_with_context::<ComponentHostCapability>(&load, LoaderContext { held });
        self.issued += 1;
        pending
    }

    #[handler::response]
    fn on_loaded(&mut self, ctx: &mut WasmCtx<'_>, result: LoadResult, LoaderContext { held }: LoaderContext) {
        self.relayed.push(result);
        held.answer(ctx, self.relayed.last().expect("the answer was just kept"));
    }
}

aether_actor::export!(public = [Loader]);
