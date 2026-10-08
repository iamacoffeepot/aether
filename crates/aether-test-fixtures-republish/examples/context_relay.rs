//! Issue 7653: a guest that asks a party it keeps a reference to, and answers
//! its own caller when the party replies.
//!
//! - `ContextRelay` (`test.context.relay`, root) keeps the sender of a
//!   `RelayJoin` as a `RelayParty` reference. A `RelayQuery` holds its reply,
//!   asks the party through that reference with the held reply in the
//!   request's context, and the party's `RelayAskResult` handler answers the
//!   caller. With nobody joined it answers `Nobody` at once.
//! - `ContextParty` (`test.context.party`, root) answers each `RelayAsk` with
//!   the question plus `RELAY_PARTY_OFFSET`, and joins the relay on a
//!   `RelayIntroduce`.

use aether_actor::{ActorInitError, Held, Pending, ProtocolRef, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    RELAY_PARTY_OFFSET, RelayAsk, RelayAskResult, RelayIntroduce, RelayJoin, RelayParty, RelayQuery, RelayQueryResult,
};

/// Carried across the party's answer: the reply the relay owes its caller.
#[aether_data::kind(name = "aether.test_fixtures.relay_context")]
struct RelayContext {
    held: Held<RelayQueryResult>,
}

/// Who the relay asks.
enum Party {
    Nobody,
    Joined(ProtocolRef<RelayParty>),
}

/// Asks the party that joined it on behalf of each query.
pub struct ContextRelay {
    party: Party,
}

#[actor(root)]
impl WasmActor for ContextRelay {
    const NAMESPACE: &'static str = "test.context.relay";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContextRelay { party: Party::Nobody })
    }

    #[handler::tell]
    fn on_join(&mut self, ctx: &mut WasmCtx<'_>, _join: RelayJoin) {
        let Some(sender) = ctx.sender() else {
            return;
        };
        let Some(party) = ctx.cast::<RelayParty>(sender) else {
            return;
        };

        self.party = Party::Joined(party);
    }

    #[handler::request]
    fn on_query(&mut self, ctx: &mut WasmCtx<'_>, query: RelayQuery) -> Pending<RelayQueryResult> {
        let (pending, held) = ctx.hold::<RelayQueryResult>();

        match self.party {
            Party::Nobody => held.answer(ctx, &RelayQueryResult::Nobody),
            Party::Joined(party) => {
                let _ = ctx.send_to_with_context(party, &RelayAsk { question: query.question }, RelayContext { held });
            }
        }

        pending
    }

    #[handler::response]
    fn on_answer(&mut self, ctx: &mut WasmCtx<'_>, result: RelayAskResult, RelayContext { held }: RelayContext) {
        held.answer(ctx, &RelayQueryResult::Answered { value: result.value });
    }
}

/// The party the relay asks: it covers `RelayParty`.
pub struct ContextParty;

#[actor(root, depends(ContextRelay))]
impl WasmActor for ContextParty {
    const NAMESPACE: &'static str = "test.context.party";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContextParty)
    }

    #[handler::tell]
    fn on_introduce(&mut self, ctx: &mut WasmCtx<'_>, _introduce: RelayIntroduce) {
        ctx.send::<ContextRelay>(&RelayJoin);
    }

    #[handler::request]
    fn on_ask(&mut self, _ctx: &mut WasmCtx<'_>, ask: RelayAsk) -> RelayAskResult {
        RelayAskResult { value: ask.question + RELAY_PARTY_OFFSET }
    }
}

aether_actor::export!(public = [ContextRelay, ContextParty]);
