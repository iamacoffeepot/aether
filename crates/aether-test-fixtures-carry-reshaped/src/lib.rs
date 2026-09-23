//! Issue 6429 companion to the bundle's `correlation_carry`: the same
//! `test.carry.requester` handler rows, so it passes the ADR-0231 §5 contract
//! check, but its `CarriedContext` gains a `generation` field. Reshaping the
//! schema changes `Kind::ID`, so this module does not declare the context
//! kind the bundle's requester carries, and replacing that requester with
//! this one while a request is pending must be refused.

#![allow(clippy::unused_self)] // aether-suppression-request: the ADR-0033 dispatch ABI fixes the handler signature at `&mut self`, and `CarryRequester` is stateless — the same allow the bundle's `correlation_carry` carries

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    CarriedReplyMatched, CarriedRequestResult, RunCarriedRequest, SubstrateHarnessObserver,
};

#[aether_data::kind(name = "aether.test_fixtures.carried_context", no_serde)]
struct CarriedContext {
    tag: u32,
    generation: u32,
}

/// Sends nothing: the refusal scenario never needs the replacement to send.
pub struct CarryRequester;

#[actor(depends(SubstrateHarnessObserver))]
impl WasmActor for CarryRequester {
    const NAMESPACE: &'static str = "test.carry.requester";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(CarryRequester)
    }

    #[handler::single]
    fn on_run(&mut self, _ctx: &mut WasmCtx<'_>, _run: RunCarriedRequest) {}

    #[handler::single]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: CarriedRequestResult) {
        if ctx.take_context::<CarriedContext>().is_some_and(|context| context.tag == reply.tag) {
            ctx.actor::<SubstrateHarnessObserver>().send(&CarriedReplyMatched);
        }
    }
}

aether_actor::export!(default = CarryRequester);
