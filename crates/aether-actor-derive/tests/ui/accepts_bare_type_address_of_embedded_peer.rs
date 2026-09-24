//! ADR-0119 parent-scope amendment: `#[actor]` gives a wasm actor
//! `type Resolver = Embedded`, so a caller that declares it as a dependency
//! reaches its default-named instance beneath the caller's runtime parent by
//! bare type, through `actor_ref` and the flat typed send.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.embedded_peer.ping")]
struct Ping {
    seq: u32,
}

struct Peer;

#[actor]
impl WasmActor for Peer {
    const NAMESPACE: &'static str = "test.embedded_peer.peer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

struct Caller;

#[actor(depends(Peer))]
impl WasmActor for Caller {
    const NAMESPACE: &'static str = "test.embedded_peer.caller";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn send_by_type(ctx: &mut WasmCtx<'_, Caller>) {
    let _ = ctx.actor_ref::<Peer>();
    ctx.send::<Peer>(&Ping { seq: 1 });
}

fn main() {}
