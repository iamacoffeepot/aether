//! ADR-0112: a `#[handler::manual]` FFI handler receives the `Manual`
//! ctx and issues its own reply via `OutboundReply::reply` — the
//! manual-class path compiles cleanly on the wasm expansion.

use aether_actor::{FfiCtx, Manual, OutboundReply, actor};

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

struct ManualProbe;

#[actor]
impl aether_actor::FfiActor for ManualProbe {
    const NAMESPACE: &'static str = "manual_probe";

    fn init<C>(_ctx: &mut C) -> Result<Self, aether_actor::BootError>
    where
        C: aether_actor::Resolver,
    {
        Ok(ManualProbe)
    }

    #[handler::manual]
    fn on_ping(&mut self, ctx: &mut FfiCtx<'_, Manual>, ping: Ping) {
        ctx.reply(&Ack { seq: ping.seq });
    }
}

fn main() {}
