//! #7201: each intent word refuses the signature that contradicts it, and the
//! refusal names the attribute that fits. A `request` must answer, `tell` and
//! `response` answer nothing, only `response` takes a fourth parameter, and
//! the four words take no `mail` / `task` argument.

use aether_actor::actor;

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.intent_refusals.ping")]
struct Ping {
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
#[kind(name = "test.intent_refusals.pong")]
struct Pong {
    seq: u32,
}

struct SilentRequest;

#[actor]
impl aether_actor::WasmActor for SilentRequest {
    const NAMESPACE: &'static str = "test.intent_refusals.silent_request";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(SilentRequest)
    }

    #[handler::request]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

struct AnsweringTell;

#[actor]
impl aether_actor::WasmActor for AnsweringTell {
    const NAMESPACE: &'static str = "test.intent_refusals.answering_tell";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(AnsweringTell)
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}

struct AnsweringResponse;

#[actor]
impl aether_actor::WasmActor for AnsweringResponse {
    const NAMESPACE: &'static str = "test.intent_refusals.answering_response";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(AnsweringResponse)
    }

    #[handler::response]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}

struct ContextOnEvent;

#[actor]
impl aether_actor::WasmActor for ContextOnEvent {
    const NAMESPACE: &'static str = "test.intent_refusals.context_on_event";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(ContextOnEvent)
    }

    #[handler::event]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping, _context: Pong) {}
}

struct TaskTell;

#[actor]
impl aether_actor::WasmActor for TaskTell {
    const NAMESPACE: &'static str = "test.intent_refusals.task_tell";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(TaskTell)
    }

    #[handler::tell(task)]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
