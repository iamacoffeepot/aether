//! Issue 7109: the first version of the carry family, which
//! `republish_carry_v2` republishes. Its rows follow the bundle's
//! `correlation_carry` (issue 6400) and `held_carry` (issue 6983) actors,
//! under `test.republish.carry.*`.
//!
//! - `CarryRequester` sends one `CarriedRequest` per `RunCarriedRequest` to
//!   the shared `ReplyHolder`, binding a `CarriedContext` with the same tag,
//!   reports `CarriedReplyMatched` when a reply recovers its own request's
//!   context, and answers `CountQuery` with the number still unanswered.
//! - `HeldRelay` answers each `HeldRequest` through a `Held` it carries in the
//!   context of a `CarriedRequest` to the holder, and answers `CountQuery`
//!   with the number of replies it still owes.
//! - `HeldRequester` sends one detached `HeldRequest` per `RunHeldRequest` to
//!   the relay, whatever its `target` names, reports `HeldReplyMatched` on
//!   each reply that echoes a tag it sent, and answers `CountQuery` with the
//!   count of matches.

use aether_actor::{ActorInitError, Held, Pending, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    CarriedReplyMatched, CarriedRequest, CarriedRequestResult, CountQuery, CountReport, HeldReplyMatched, HeldRequest,
    HeldRequestResult, RunCarriedRequest, RunHeldRequest, SubstrateHarnessObserver,
};
use aether_test_fixtures_republish::ReplyHolder;

/// The context one carried request binds. v2 adds a field, changing its
/// `Kind::ID`.
#[aether_data::kind(name = "aether.test_fixtures.republish_carried_context", no_serde)]
struct CarriedContext {
    tag: u32,
}

/// The state one relayed request carries to its reply handler: the reply
/// the relay owes, and the tag it echoes. v2 changes the held reply kind,
/// changing its `Kind::ID`.
#[aether_data::kind(name = "aether.test_fixtures.republish_held_relay_context")]
struct HeldRelayContext {
    held: Held<HeldRequestResult>,
    tag: u32,
}

/// Sends nothing from `init` or `wire`, so its first request takes the first
/// id its mailbox mints. It answers `CountQuery` with the number of its
/// requests whose replies have not matched, and carries that count across a
/// replace.
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

    #[handler::single]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunCarriedRequest) {
        let _ = ctx.send_with_context::<ReplyHolder>(&CarriedRequest { tag: run.tag }, CarriedContext { tag: run.tag });
        self.unanswered += 1;
    }

    #[handler::single]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: CarriedRequestResult) {
        if ctx.take_context::<CarriedContext>().is_some_and(|context| context.tag == reply.tag) {
            self.unanswered = self.unanswered.saturating_sub(1);
            ctx.send::<SubstrateHarnessObserver>(&CarriedReplyMatched);
        }
    }

    #[handler::single]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.unanswered }
    }
}

/// Holds each request's reply in the context of a `CarriedRequest` to the
/// holder, and answers when that request's result comes back. It answers
/// `CountQuery` with the number of replies it still owes, and carries that
/// count across a replace.
pub struct HeldRelay {
    owed: u32,
}

#[actor(root, depends(ReplyHolder))]
impl WasmActor for HeldRelay {
    const NAMESPACE: &'static str = "test.republish.carry.held_relay";

    type State = CountReport;

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HeldRelay { owed: 0 })
    }

    fn dehydrate(&self) -> CountReport {
        CountReport { count: self.owed }
    }

    fn rehydrate(&mut self, CountReport { count }: CountReport) {
        self.owed = count;
    }

    #[handler::single]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        let _ = ctx.send_with_context::<ReplyHolder>(
            &CarriedRequest { tag: request.tag },
            HeldRelayContext { held, tag: request.tag },
        );
        self.owed += 1;
        pending
    }

    #[handler::single]
    fn on_result(&mut self, ctx: &mut WasmCtx<'_>, _result: CarriedRequestResult) {
        if let Some(context) = ctx.take_context::<HeldRelayContext>() {
            self.owed = self.owed.saturating_sub(1);
            context.held.answer(ctx, &HeldRequestResult { tag: context.tag });
        }
    }

    #[handler::single]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.owed }
    }
}

/// Sends each `HeldRequest` detached, so the held reply's chain stays out of
/// the chain a harness settles, and counts the replies that echo a tag it
/// sent.
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

    #[handler::single]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunHeldRequest) {
        ctx.send_detached::<HeldRelay>(&HeldRequest { tag: run.tag });
        self.sent.push(run.tag);
    }

    #[handler::single]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: HeldRequestResult) {
        if let Some(index) = self.sent.iter().position(|tag| *tag == reply.tag) {
            self.sent.swap_remove(index);
            self.matched += 1;
            ctx.send::<SubstrateHarnessObserver>(&HeldReplyMatched);
        }
    }

    #[handler::single]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.matched }
    }
}

aether_actor::export!(public = [CarryRequester, HeldRelay, HeldRequester, ReplyHolder]);
