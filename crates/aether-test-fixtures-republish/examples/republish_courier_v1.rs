//! Issue 7086: the first version of the courier pair, which
//! `republish_courier_v2` republishes.
//!
//! - `Courier` (`test.republish.courier`, root) carries `CourierState` across
//!   a replace, so its successor's `on_rehydrate` runs, and answers
//!   `CourierQuery` with no outcomes: this version mails nobody.
//! - `Parcel` (`test.republish.parcel`, instanced) counts the `CountQuery`s
//!   it answers; it is what the successor courier loads and drops.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    CountQuery, CountReport, CourierConfig, CourierQuery, CourierQueryResult, CourierState,
};

pub struct Courier {
    hops: u32,
    /// Always empty: this version mails nobody, so it hears no answers.
    outcomes: Vec<String>,
}

#[actor(root)]
impl WasmActor for Courier {
    type Config = CourierConfig;
    const NAMESPACE: &'static str = "test.republish.courier";

    type State = CourierState;

    fn init(_config: CourierConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Courier { hops: 0, outcomes: Vec::new() })
    }

    fn dehydrate(&self) -> CourierState {
        CourierState { hops: self.hops }
    }

    fn rehydrate(&mut self, CourierState { hops }: CourierState) {
        self.hops = hops;
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CourierQuery) -> CourierQueryResult {
        CourierQueryResult { outcomes: self.outcomes.clone() }
    }
}

/// Counts the `CountQuery`s it has answered.
pub struct Parcel {
    queries: u32,
}

#[actor(instanced, root)]
impl WasmActor for Parcel {
    const NAMESPACE: &'static str = "test.republish.parcel";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Parcel { queries: 0 })
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        self.queries += 1;
        CountReport { count: self.queries }
    }
}

aether_actor::export!(public = [Courier, Parcel]);
