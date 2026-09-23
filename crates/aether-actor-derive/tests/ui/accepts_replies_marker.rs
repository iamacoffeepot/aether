//! ADR-0227: handler signatures emit typed reply markers beside
//! `HandlesKind`: a single handler returning `R` implements `Replies` with
//! `Reply = R`.

use aether_actor::{Replies, WasmCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.reply_marker.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.reply_marker.pong")]
struct Pong {
    seq: u32,
}

struct ReplyProbe;

#[actor]
impl aether_actor::WasmActor for ReplyProbe {
    const NAMESPACE: &'static str = "reply_marker_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}

fn assert_replies<T: Replies<Ping, Reply = Pong>>() {}

fn main() {
    assert_replies::<ReplyProbe>();
}
