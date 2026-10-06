//! ADR-0079 §8: a wasm actor watches a typed reference it holds and takes the
//! departure as `Departed<W>`, with the context its handler names.
//!
//! One handler is for a protocol and takes a context; one is for an actor
//! type and takes none, so its watches pass `NoContext`. Both `ctx.watch`
//! calls compile only if `#[actor]` emitted `Watches<W>` with that context
//! kind, and the two handlers compile side by side only if they share one
//! row for the notice rather than each claiming its kind. The event's
//! reference is the type the target was watched through.

use aether_actor::{
    ActorInitError, ActorRef, Departed, NoContext, ProtocolRef, WasmActor, WasmCtx, WasmInitCtx, WatchId, actor,
    protocol,
};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.watch_handlers.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.watch_handlers.poke")]
struct Poke {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.watch_handlers.note")]
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
    const NAMESPACE: &'static str = "test.watch_handlers.camera";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Camera)
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

struct Ledger {
    provider: Option<ProtocolRef<Provider>>,
    camera: Option<ActorRef<Camera>>,
    watched: Option<WatchId>,
    gone: u32,
}

#[actor]
impl WasmActor for Ledger {
    const NAMESPACE: &'static str = "test.watch_handlers.ledger";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Ledger { provider: None, camera: None, watched: None, gone: 0 })
    }

    #[handler::tell]
    fn on_poke(&mut self, ctx: &mut WasmCtx<'_>, poke: Poke) {
        if let Some(provider) = self.provider {
            self.watched = Some(ctx.watch(provider, Note { seq: poke.seq }));
        }
        if let Some(camera) = self.camera {
            let watch = ctx.watch(camera, NoContext);
            ctx.unwatch(watch);
        }
    }

    #[handler::event]
    fn on_provider_gone(&mut self, _ctx: &mut WasmCtx<'_>, event: Departed<Provider>, note: Note) {
        let departed: ProtocolRef<Provider> = event.actor;
        if self.provider == Some(departed) && self.watched == Some(event.watch) {
            self.gone += note.seq;
        }
    }

    #[handler::event]
    fn on_camera_gone(&mut self, _ctx: &mut WasmCtx<'_>, event: Departed<Camera>) {
        let departed: ActorRef<Camera> = event.actor;
        if self.camera == Some(departed) {
            self.camera = None;
        }
    }
}

fn main() {}
