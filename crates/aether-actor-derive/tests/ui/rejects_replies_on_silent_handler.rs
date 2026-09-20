//! ADR-0227: a fire-and-forget `-> ()` handler emits `HandlesKind<K>` but no
//! `Replies<K>`, so a typed request helper rejects it.

use aether_actor::{Replies, WasmCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.replies.silent.ping")]
struct Ping {
    seq: u32,
}

struct SilentProbe;

#[actor]
impl aether_actor::WasmActor for SilentProbe {
    const NAMESPACE: &'static str = "silent_reply_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

fn assert_replies<T: Replies<Ping>>() {}

fn main() {
    assert_replies::<SilentProbe>();
}
