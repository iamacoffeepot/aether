//! #7201: the four intent words compile on a wasm actor and on a
//! `#[handler_set]` member, in every arm shape they generate.
//!
//! `request` answers synchronously (`-> O`) and deferred (`-> Pending<O>`),
//! `tell` and `event` answer nothing, and `response` takes no context, a
//! `context: C`, or a `context: Option<C>` (ADR-0243 §10). The `C` and
//! `Option<C>` forms emit the context take the arm runs before the call, so a
//! wrong take, a wrong argument position, or a wrong absent-branch return code
//! is a type error here. The set member proves the same take inside the set's
//! own dispatch chain.

use aether_actor::{actor, handler_set};

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.ask")]
struct Ask {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.later")]
struct Later {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.answer")]
struct Answer {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.poke")]
struct Poke {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.tick")]
struct Tick {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.bare_reply")]
struct BareReply {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.required_reply")]
struct RequiredReply {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.optional_reply")]
struct OptionalReply {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.set_reply")]
struct SetReply {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_intents.context")]
struct AskContext {
    seq: u32,
}

#[handler_set]
trait SharedReplies {
    fn seen(&mut self) -> &mut u32;

    #[handler::response]
    fn on_set_reply(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, reply: SetReply, context: AskContext) {
        *self.seen() += reply.seq + context.seq;
    }
}

struct Intents {
    seen: u32,
}

impl SharedReplies for Intents {
    fn seen(&mut self) -> &mut u32 {
        &mut self.seen
    }
}

#[actor(handler_set(SharedReplies))]
impl aether_actor::WasmActor for Intents {
    const NAMESPACE: &'static str = "test.handler_intents";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Intents { seen: 0 })
    }

    #[handler::request]
    fn on_ask(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, ask: Ask) -> Answer {
        Answer { seq: ask.seq + self.seen }
    }

    #[handler::request]
    fn on_later(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _later: Later) -> aether_actor::Pending<Answer> {
        unimplemented!("a pass fixture is compiled, never dispatched")
    }

    #[handler::tell]
    fn on_poke(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, poke: Poke) {
        self.seen = poke.seq;
    }

    #[handler::event]
    fn on_tick(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, tick: Tick) {
        self.seen += tick.seq;
    }

    #[handler::response]
    fn on_bare_reply(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, reply: BareReply) {
        self.seen += reply.seq;
    }

    #[handler::response]
    fn on_required_reply(
        &mut self,
        _ctx: &mut aether_actor::WasmCtx<'_>,
        reply: RequiredReply,
        context: AskContext,
    ) {
        self.seen += reply.seq + context.seq;
    }

    #[handler::response]
    fn on_optional_reply(
        &mut self,
        _ctx: &mut aether_actor::WasmCtx<'_>,
        reply: OptionalReply,
        context: Option<AskContext>,
    ) {
        self.seen += reply.seq + context.map_or(0, |c| c.seq);
    }
}

fn main() {}
