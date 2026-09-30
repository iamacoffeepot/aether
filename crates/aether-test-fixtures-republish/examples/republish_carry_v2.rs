//! Issue 7109: the second version of the carry family, republishing
//! `republish_carry_v1` with the same rows and two reshaped context kinds.
//!
//! - `CarryRequester`'s `CarriedContext` gains a `generation` field, so its
//!   `Kind::ID` changes, and a republish while a v1 context is carried must be
//!   refused (issue 6429).
//! - `HeldRelay`'s `HeldRelayContext` holds a `Held<CarriedRequestResult>`
//!   where v1's holds a `Held<HeldRequestResult>`. A held field's schema names
//!   its reply kind (ADR-0243 §4), so its `Kind::ID` changes, and a republish
//!   while a v1 held reply is carried must be refused (issue 6983). This relay
//!   carries nothing: it answers each request at once.
//! - `HeldRequester` is v1's, sending to this version's relay.

use aether_actor::{ActorInitError, Held, Pending, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    CarriedReplyMatched, CarriedRequest, CarriedRequestResult, CountQuery, CountReport, HeldReplyMatched, HeldRequest,
    HeldRequestResult, RunCarriedRequest, RunHeldRequest, SubstrateHarnessObserver,
};
use aether_test_fixtures_republish::ReplyHolder;

/// v1's carried context, name kept, with an added field.
#[aether_data::kind(name = "aether.test_fixtures.republish_carried_context", no_serde)]
struct CarriedContext {
    tag: u32,
    generation: u32,
}

/// The generation this version's requester binds.
const GENERATION: u32 = 2;

/// v1's held relay context, name and fields kept, with the held field's reply
/// kind changed.
#[aether_data::kind(name = "aether.test_fixtures.republish_held_relay_context")]
struct HeldRelayContext {
    held: Held<CarriedRequestResult>,
    tag: u32,
}

/// v1's requester, binding the reshaped context.
pub struct CarryRequester {
    unanswered: u32,
}

#[actor(root, depends(ReplyHolder, SubstrateHarnessObserver))]
impl WasmActor for CarryRequester {
    const NAMESPACE: &'static str = "test.republish.carry.requester";

    type State = CountReport;

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(CarryRequester { unanswered: 0 })
    }

    fn dehydrate(&self) -> CountReport {
        CountReport { count: self.unanswered }
    }

    fn rehydrate(&mut self, CountReport { count }: CountReport) {
        self.unanswered = count;
    }

    #[handler::tell]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunCarriedRequest) {
        let _ = ctx.send_with_context::<ReplyHolder>(
            &CarriedRequest { tag: run.tag },
            CarriedContext { tag: run.tag, generation: GENERATION },
        );
        self.unanswered += 1;
    }

    #[handler::response]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: CarriedRequestResult, context: Option<CarriedContext>) {
        if context.is_some_and(|context| context.tag == reply.tag && context.generation == GENERATION) {
            self.unanswered = self.unanswered.saturating_sub(1);
            ctx.send::<SubstrateHarnessObserver>(&CarriedReplyMatched);
        }
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.unanswered }
    }
}

/// v1's rows. It answers each request at once, answers a carried context's
/// held reply with the result that context's request brought back, and
/// answers `CountQuery` with the number of replies it has answered.
pub struct HeldRelay {
    answered: u32,
}

#[actor(root, depends(ReplyHolder))]
impl WasmActor for HeldRelay {
    const NAMESPACE: &'static str = "test.republish.carry.held_relay";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HeldRelay { answered: 0 })
    }

    #[handler::request]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        held.answer(ctx, &HeldRequestResult { tag: request.tag });
        self.answered += 1;
        pending
    }

    #[handler::response]
    fn on_result(&mut self, ctx: &mut WasmCtx<'_>, result: CarriedRequestResult, context: HeldRelayContext) {
        self.answered += 1;
        context.held.answer(ctx, &result);
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.answered }
    }
}

/// v1's held requester, sending to this version's relay.
pub struct HeldRequester {
    sent: Vec<u32>,
    matched: u32,
}

#[actor(root, depends(HeldRelay, SubstrateHarnessObserver))]
impl WasmActor for HeldRequester {
    const NAMESPACE: &'static str = "test.republish.carry.held_requester";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HeldRequester { sent: Vec::new(), matched: 0 })
    }

    #[handler::tell]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunHeldRequest) {
        ctx.send_detached::<HeldRelay>(&HeldRequest { tag: run.tag });
        self.sent.push(run.tag);
    }

    #[handler::response]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: HeldRequestResult) {
        if let Some(index) = self.sent.iter().position(|tag| *tag == reply.tag) {
            self.sent.swap_remove(index);
            self.matched += 1;
            ctx.send::<SubstrateHarnessObserver>(&HeldReplyMatched);
        }
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.matched }
    }
}

aether_actor::export!(public = [CarryRequester, HeldRelay, HeldRequester, ReplyHolder]);
