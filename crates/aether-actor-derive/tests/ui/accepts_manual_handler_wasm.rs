//! ADR-0112: a `#[handler::manual]` FFI handler receives the `Manual`
//! ctx and issues its own reply via `OutboundReply::reply` — the
//! manual-class path compiles cleanly on the wasm expansion.
//!
//! The native manual-class behavior is covered by the
//! `manual_handler_replies_through_ctx` integration test in
//! `aether-substrate` (this proc-macro crate has no `aether-substrate`
//! dev-dep, so a native *pass* / type-error fixture can't link the
//! substrate types — the existing native fixtures here are all
//! macro-level diagnostics that fire before path resolution).

use aether_actor::{Manual, OutboundReply, WasmCtx, actor};

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

struct ManualProbe;

#[actor]
impl aether_actor::WasmActor for ManualProbe {
    const NAMESPACE: &'static str = "manual_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(ManualProbe)
    }

    #[handler::manual]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_, Manual>, ping: Ping) {
        ctx.reply(&Ack { seq: ping.seq });
    }
}

fn main() {}
