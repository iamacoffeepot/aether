//! Issue 6429 companion to the bundle's `correlation_carry`: the same handler
//! rows as `test.carry.requester`, so it passes the ADR-0231 §5 contract
//! check, but its `CarriedContext` gains a `generation` field. Reshaping the
//! schema changes `Kind::ID`, so this module does not declare the context
//! kind the bundle's requester carries, and replacing that requester with
//! this one while a request is pending must be refused.
//!
//! It exports its own namespace, `test.carry.reshaped_requester`, and the
//! scenario swaps the requester to it by export. Under ADR-0241 §3 a module
//! that exported `test.carry.requester` alone would republish the bundle's
//! namespace while dropping the bundle's others, which publish admission
//! refuses before the carried-context check could run.
//!
//! Issue 6983 companion to the bundle's `held_carry`: `ReshapedHeldRelay` has
//! the same handler rows as `test.held.relay`, but its `HeldRelayContext`
//! holds a `Held<CarriedRequestResult>` where the bundle's holds a
//! `Held<HeldRequestResult>`. A held field's schema names its reply kind (ADR-0243
//! §4), so the context's `Kind::ID` changes, and swapping the bundle's relay
//! to this export while a held reply is carried must be refused.

#![allow(clippy::unused_self)] // aether-suppression-request: the ADR-0033 dispatch ABI fixes the handler signature at `&mut self`, and `CarryRequester` is stateless — the same allow the bundle's `correlation_carry` carries

use aether_actor::{ActorInitError, Held, Pending, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    CarriedReplyMatched, CarriedRequestResult, HeldRequest, HeldRequestResult, RunCarriedRequest,
    SubstrateHarnessObserver,
};

#[aether_data::kind(name = "aether.test_fixtures.carried_context", no_serde)]
struct CarriedContext {
    tag: u32,
    generation: u32,
}

/// Sends nothing: the refusal scenario never needs the replacement to send.
pub struct CarryRequester;

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for CarryRequester {
    const NAMESPACE: &'static str = "test.carry.reshaped_requester";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(CarryRequester)
    }

    #[handler::single]
    fn on_run(&mut self, _ctx: &mut WasmCtx<'_>, _run: RunCarriedRequest) {}

    #[handler::single]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: CarriedRequestResult) {
        if ctx.take_context::<CarriedContext>().is_some_and(|context| context.tag == reply.tag) {
            ctx.send::<SubstrateHarnessObserver>(&CarriedReplyMatched);
        }
    }
}

/// The bundle's held relay context, name and fields kept, with the held
/// field's reply kind changed.
#[aether_data::kind(name = "aether.test_fixtures.held_relay_context")]
struct HeldRelayContext {
    held: Held<CarriedRequestResult>,
    tag: u32,
}

/// The rows of the bundle's `test.held.relay`. The refusal scenario never
/// installs it, so it answers each request at once.
pub struct ReshapedHeldRelay;

#[actor(root)]
impl WasmActor for ReshapedHeldRelay {
    const NAMESPACE: &'static str = "test.held.reshaped_relay";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ReshapedHeldRelay)
    }

    #[handler::single]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        held.answer(ctx, &HeldRequestResult { tag: request.tag });
        pending
    }

    #[handler::single]
    fn on_result(&mut self, ctx: &mut WasmCtx<'_>, result: CarriedRequestResult) {
        if let Some(context) = ctx.take_context::<HeldRelayContext>() {
            context.held.answer(ctx, &result);
        }
    }
}

aether_actor::export!(default = CarryRequester, public = [ReshapedHeldRelay]);
