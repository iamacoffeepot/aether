//! Issue 7125: the second version of the keep pair, republishing
//! `republish_keep_v1`.
//!
//! - `Keeper` is the shared one, unchanged.
//! - `Refuser` traps in `on_rehydrate`, so its candidate refuses the prepare
//!   whenever its predecessor saved a bundle, which v1's always does, and the
//!   group aborts after every other member is ready.

use std::process;

use aether_actor::{ActorInitError, PriorState, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{CountQuery, CountReport};
use aether_test_fixtures_republish::Keeper;

pub struct Refuser {
    count: u32,
}

#[actor(root)]
impl WasmActor for Refuser {
    const NAMESPACE: &'static str = "test.republish.keep.refuser";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Refuser { count: 0 })
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }

    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        ctx.save_state_kind(0, &CountReport { count: self.count });
    }

    /// `abort` lowers to `unreachable`, which the host reports as an
    /// `on_rehydrate` failure.
    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, _prior: PriorState<'_>) {
        process::abort();
    }
}

aether_actor::export!(public = [Keeper, Refuser]);
