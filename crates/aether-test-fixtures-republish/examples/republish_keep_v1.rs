//! Issue 7125: the first version of the keep pair, which `republish_keep_v2`
//! republishes.
//!
//! - `Keeper` (`test.republish.keep.keeper`, root, shared) holds each
//!   `HeldRequest`'s reply and moves the first one, with its request count,
//!   out of itself in `on_dehydrate`.
//! - `Refuser` (`test.republish.keep.refuser`, root) saves a `CountReport` in
//!   `on_dehydrate`, so a republish carries a bundle to its successor, restores
//!   nothing, and answers `CountQuery` with the count it never raises.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor};
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

    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
        ctx.save_state_kind(0, &CountReport { count: self.count })
    }
}

aether_actor::export!(public = [Keeper, Refuser]);
