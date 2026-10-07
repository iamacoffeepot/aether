//! ADR-0112: a `#[handler::unchecked(..)]` FFI handler receives the `Unchecked`
//! ctx and issues its own reply via `OutboundReply::reply` — the
//! unchecked-class path compiles cleanly on the wasm expansion.
//!
//! The native unchecked-class behavior is covered by the
//! `unchecked_handler_replies_through_ctx` integration test in
//! `aether-substrate` (this proc-macro crate has no `aether-substrate`
//! dev-dep, so a native *pass* / type-error fixture can't link the
//! substrate types — the existing native fixtures here are all
//! macro-level diagnostics that fire before path resolution).

use aether_actor::{Anyone, Erased, OutboundReply, Unchecked, WasmCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.ack")]
struct Ack {
    seq: u32,
}

struct UncheckedProbe;

#[actor]
impl aether_actor::WasmActor for UncheckedProbe {
    const NAMESPACE: &'static str = "unchecked_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(UncheckedProbe)
    }

    #[handler::unchecked(reason = "test: replies by hand")]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, ping: Ping) {
        ctx.reply(&Ack { seq: ping.seq });
    }
}

fn main() {}
