//! Issue 7535: the second version of the hooks pair, republishing
//! `republish_hooks_v1`. It keeps v1's namespaces and rows.
//!
//! - `Parent` keeps the default replace hooks, so it takes whatever v1 saved.
//! - `Counter` writes `on_rehydrate` by hand and returns an error from it, so
//!   the child v1 saved cannot be rebuilt and the republish is refused
//!   (ADR-0249 §6).

use aether_actor::{
    ActorInitError, Held, Pending, PriorState, Subname, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor,
};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, HeldRequest, HeldRequestResult, HookFaultConfig, REHYDRATE_REFUSAL,
};

pub struct Parent {
    count: u32,
    held: Vec<Held<HeldRequestResult>>,
}

#[actor(root, spawns(Counter))]
impl WasmActor for Parent {
    type Config = HookFaultConfig;
    const NAMESPACE: &'static str = "test.republish.hooks.parent";

    fn init(_config: HookFaultConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Parent { count: 0, held: Vec::new() })
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.spawn_inline::<Counter>(Subname::Named("counter"), &())
            .map(drop)
            .map_err(|error| ActorInitError::new(format!("the counter does not spawn: {error:?}")))
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }

    #[handler::request]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, _request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        self.held.push(held);
        pending
    }
}

pub struct Counter {
    count: u32,
}

#[actor(instanced, child_of(Parent))]
impl WasmActor for Counter {
    const NAMESPACE: &'static str = "test.republish.hooks.counter";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Counter { count: 0 })
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, _prior: PriorState<'_>) -> Result<(), ActorInitError> {
        Err(ActorInitError::new(REHYDRATE_REFUSAL))
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }
}

aether_actor::export!(public = [Parent], private = [Counter]);
