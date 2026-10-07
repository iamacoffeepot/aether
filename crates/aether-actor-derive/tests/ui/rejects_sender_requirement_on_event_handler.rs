//! ADR-0231 §11: only a `#[handler::tell]` and a `#[handler::request]` name a
//! sender in their ctx type. An event's sender is a publisher the actor
//! chose, a response's is whoever the actor asked, an unchecked handler has
//! no declared reply the engine could refuse a request through, a `#[fallback]`
//! catches mail no send names, and a lifecycle hook dispatches no mail, so
//! each is refused by the macro in its own words. Accepting one would emit a
//! cast that refuses mail from the very actor the handler exists to hear, or
//! a requirement no send could ever be checked against.

use aether_actor::{ActorInitError, Mail, Unchecked, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

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
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_, Self, Listener>, _mail: Tick) {}
}

struct OnResponse;

#[actor(root)]
impl WasmActor for OnResponse {
    const NAMESPACE: &'static str = "test.sender_intent.on_response";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::response]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_, Self, Listener>, _mail: Tick) {}
}

struct OnUnchecked;

#[actor(root)]
impl WasmActor for OnUnchecked {
    const NAMESPACE: &'static str = "test.sender_intent.on_unchecked";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::unchecked(reason = "test: replies by hand")]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_, Self, Listener, Unchecked>, _mail: Tick) {}
}

struct OnFallback;

#[actor(root)]
impl WasmActor for OnFallback {
    const NAMESPACE: &'static str = "test.sender_intent.on_fallback";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_any(&mut self, _ctx: &mut WasmCtx<'_, Self, Listener>, _mail: Mail<'_>) {}
}

struct OnHook;

#[actor(root)]
impl WasmActor for OnHook {
    const NAMESPACE: &'static str = "test.sender_intent.on_hook";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    fn unwire(&mut self, _ctx: &mut WasmCtx<'_, Self, Listener>) {}
}

fn main() {}
