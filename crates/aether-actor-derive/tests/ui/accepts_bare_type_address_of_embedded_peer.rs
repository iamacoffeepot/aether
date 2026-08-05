//! ADR-0119 parent-scope amendment: `#[actor]` gives a wasm actor
//! `type Resolver = Embedded`, so ordinary typed ctx and `MailSender` sends
//! resolve its default-named instance beneath the caller's runtime parent.

use aether_actor::{ActorInitError, Mail, MailSender, WasmActor, WasmCtx, WasmInitCtx, actor};

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

#[actor]
impl WasmActor for Caller {
    const NAMESPACE: &'static str = "test.embedded_peer.caller";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn address_by_type(ctx: &WasmCtx<'_>) {
    let _ = ctx.actor::<Peer>();
}

fn send_by_type(ctx: &mut WasmCtx<'_>) {
    MailSender::send::<Peer, Ping>(ctx, &Ping { seq: 1 });
}

fn main() {}
