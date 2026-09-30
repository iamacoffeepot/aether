//! Issue 7086: the second version of the courier pair, republishing
//! `republish_courier_v1`.
//!
//! - `Courier` adds `depends(ComponentHostCapability)` and, from
//!   `on_rehydrate`, mails the component host what its `CourierConfig`
//!   names: a load of `wasm` as a `test.republish.parcel` keyed `late`, and a
//!   drop of `drop`. `on_rehydrate` runs while the candidate's outbox is
//!   held, so both leave when the republish commits, on that commit's chain.
//!   It records each answer, and `CourierQuery` reads them back.
//! - `Parcel` is unchanged.

use aether_actor::{ActorInitError, PriorState, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor};
use aether_component::ComponentHostCapability;
use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult};
use aether_test_fixtures_kinds::{
    CountQuery, CountReport, CourierConfig, CourierQuery, CourierQueryResult, CourierState,
};

pub struct Courier {
    config: CourierConfig,
    outcomes: Vec<String>,
}

#[actor(root, depends(ComponentHostCapability))]
impl WasmActor for Courier {
    type Config = CourierConfig;
    const NAMESPACE: &'static str = "test.republish.courier";

    fn init(config: CourierConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Courier { config, outcomes: Vec::new() })
    }

    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        ctx.save_state_kind::<CourierState>(0, &CourierState::default());
    }

    /// Mail the component host what the config names. Nothing leaves until
    /// the republish commits.
    fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_>, _prior: PriorState<'_>) {
        if !self.config.wasm.is_empty() {
            let load = LoadComponent {
                wasm: self.config.wasm.clone(),
                name: Some("late".to_owned()),
                config: Vec::new(),
                export: Some("test.republish.parcel".to_owned()),
            };
            ctx.send::<ComponentHostCapability>(&load);
        }
        if let Some(target) = self.config.drop.clone() {
            ctx.send::<ComponentHostCapability>(&DropComponent { target });
        }
    }

    #[handler::response]
    fn on_loaded(&mut self, _ctx: &mut WasmCtx<'_>, result: LoadResult) {
        self.outcomes.push(match result {
            LoadResult::Ok { path, .. } => format!("load ok {path}"),
            LoadResult::Err { error } => format!("load err {error}"),
        });
    }

    #[handler::response]
    fn on_dropped(&mut self, _ctx: &mut WasmCtx<'_>, result: DropResult) {
        self.outcomes.push(match result {
            DropResult::Ok => "drop ok".to_owned(),
            DropResult::Err { error } => format!("drop err {error}"),
        });
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
