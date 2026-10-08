//! ADR-0079 §8: `ctx.watch` compiles only for a typed reference whose type
//! the actor has a departure handler for, with the context kind that handler
//! takes. A watch through a type no handler names would never be handled, a
//! context of another kind could not be handed to the handler, and an erased
//! reference names no watched type at all.

use aether_actor::{
    ActorInitError, ActorRef, Departed, ErasedActorRef, NoContext, ProtocolRef, WasmActor, WasmCtx, WasmInitCtx,
    actor, protocol,
};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.watch_calls.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.watch_calls.note")]
struct Note {
    seq: u32,
}

#[protocol]
trait Provider {
    fn ping(_: Ping);
}

struct Camera;

#[actor]
impl WasmActor for Camera {
    const NAMESPACE: &'static str = "test.watch_calls.camera";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Camera)
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

struct Ledger {
    provider: Option<ProtocolRef<Provider>>,
    camera: Option<ActorRef<Camera>>,
}

#[actor]
impl WasmActor for Ledger {
    const NAMESPACE: &'static str = "test.watch_calls.ledger";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Ledger { provider: None, camera: None })
    }

    #[handler::tell]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_>, ping: Ping) {
        if let Some(camera) = self.camera {
            ctx.watch(camera, NoContext);
        }
        if let Some(provider) = self.provider {
            ctx.watch(provider, NoContext);
        }
        if let Some(sender) = ctx.sender() {
            let erased: ErasedActorRef = sender;
            ctx.watch(erased, Note { seq: ping.seq });
        }
    }

    #[handler::event]
    fn on_provider_gone(&mut self, _ctx: &mut WasmCtx<'_>, _event: Departed<Provider>, _note: Note) {}
}

fn main() {}
