//! ADR-0231 §11: only a `#[handler::tell]` and a `#[handler::request]` take
//! `sender: ProtocolRef<P>`. An event's sender is a publisher the actor chose,
//! a response's is whoever the actor asked, and an unchecked handler has no
//! declared reply the engine could refuse a request through, so each is
//! refused by the macro in its own words. Accepting one would emit a cast
//! that refuses mail from the very actor the handler exists to hear.

use aether_actor::{ActorInitError, ProtocolRef, Unchecked, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

#[aether_data::kind(name = "test.sender_intent.tick", copy)]
struct Tick;

#[aether_data::kind(name = "test.sender_intent.notice", copy)]
struct Notice;

#[protocol]
trait Listener {
    fn notice(mail: Notice);
}

struct OnEvent;

#[actor(root)]
impl WasmActor for OnEvent {
    const NAMESPACE: &'static str = "test.sender_intent.on_event";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::event]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Tick, _sender: ProtocolRef<Listener>) {}
}

struct OnResponse;

#[actor(root)]
impl WasmActor for OnResponse {
    const NAMESPACE: &'static str = "test.sender_intent.on_response";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::response]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Tick, _sender: ProtocolRef<Listener>) {}
}

struct OnUnchecked;

#[actor(root)]
impl WasmActor for OnUnchecked {
    const NAMESPACE: &'static str = "test.sender_intent.on_unchecked";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::unchecked(reason = "test: replies by hand")]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_, Self, Unchecked>, _mail: Tick, _sender: ProtocolRef<Listener>) {}
}

fn main() {}
