//! ADR-0079 §8: a departure handler's signature is refused where it would
//! contradict the watch it declares. Its context is never `Option<C>`,
//! because a watch always stores its context. One handler serves each watched
//! type, so a second for the same type is refused. It is a
//! `#[handler::event]` that answers nothing, it cannot sit beside a handler
//! for the notice its row stands for, and a handler set cannot declare one.

use aether_actor::{ActorInitError, Departed, WasmActor, WasmCtx, WasmInitCtx, actor, handler_set, protocol};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.watch_signatures.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.watch_signatures.note")]
struct Note {
    seq: u32,
}

/// Stands in for the engine's notice, which the macro knows by name.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.watch_signatures.monitor_notice")]
struct MonitorNotice {
    seq: u32,
}

#[protocol]
trait Provider {
    fn ping(_: Ping);
}

struct OptionalContext;

#[actor]
impl WasmActor for OptionalContext {
    const NAMESPACE: &'static str = "test.watch_signatures.optional_context";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(OptionalContext)
    }

    #[handler::event]
    fn on_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Provider>, _note: Option<Note>) {}
}

struct TwoHandlers;

#[actor]
impl WasmActor for TwoHandlers {
    const NAMESPACE: &'static str = "test.watch_signatures.two_handlers";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(TwoHandlers)
    }

    #[handler::event]
    fn on_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Provider>, _note: Note) {}

    #[handler::event]
    fn on_gone_again(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Provider>) {}
}

struct ToldDeparture;

#[actor]
impl WasmActor for ToldDeparture {
    const NAMESPACE: &'static str = "test.watch_signatures.told_departure";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ToldDeparture)
    }

    #[handler::tell]
    fn on_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Provider>) {}
}

struct AnsweringDeparture;

#[actor]
impl WasmActor for AnsweringDeparture {
    const NAMESPACE: &'static str = "test.watch_signatures.answering_departure";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(AnsweringDeparture)
    }

    #[handler::event]
    fn on_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Provider>) -> Note {
        Note { seq: 0 }
    }
}

struct NoticeBeside;

#[actor]
impl WasmActor for NoticeBeside {
    const NAMESPACE: &'static str = "test.watch_signatures.notice_beside";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(NoticeBeside)
    }

    #[handler::event]
    fn on_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Provider>) {}

    #[handler::event]
    fn on_notice(&mut self, _ctx: &mut WasmCtx<'_>, _notice: MonitorNotice) {}
}

#[handler_set]
trait SharedDepartures {
    #[handler::event]
    fn on_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Provider>) {}
}

fn main() {}
