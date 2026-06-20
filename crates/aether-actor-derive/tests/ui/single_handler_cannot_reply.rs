//! ADR-0112 (single-locked): a plain `#[handler]` (single-class) body has
//! no reply surface — the `Single` ctx does not implement `OutboundReply`,
//! so a hand-call to `ctx.reply` is a compile error. This locks `-> ()`
//! as provably silent (the manifest's `ReplyContract::None` is true by
//! construction). A handler that needs to reply by hand declares
//! `#[handler::manual]` and takes the `Manual` ctx.

use aether_actor::{WasmCtx, OutboundReply, actor};

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.ping")]
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
#[kind(name = "test.ack")]
struct Ack {
    seq: u32,
}

struct SilentProbe;

#[actor]
impl aether_actor::WasmActor for SilentProbe {
    const NAMESPACE: &'static str = "silent_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::BootError>
    {
        Ok(SilentProbe)
    }

    #[handler]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_>, ping: Ping) {
        // A single-class handler has no reply surface: `OutboundReply` is
        // not implemented for `WasmCtx<'_, Single>`, so this fails to compile.
        ctx.reply(&Ack { seq: ping.seq });
    }
}

fn main() {}
