//! ADR-0227: an unchecked handler can issue arbitrary replies and therefore emits
//! `HandlesKind<K>` but no `Replies<K>` marker.

use aether_actor::{Anyone, Erased, Replies, Unchecked, WasmCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.replies.unchecked.ping")]
struct Ping {
    seq: u32,
}

struct UncheckedProbe;

#[actor]
impl aether_actor::WasmActor for UncheckedProbe {
    const NAMESPACE: &'static str = "unchecked_reply_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Self)
    }

    #[handler::unchecked(reason = "test: an unchecked handler emits no Replies marker")]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, _ping: Ping) {}
}

fn assert_replies<T: Replies<Ping>>() {}

fn main() {
    assert_replies::<UncheckedProbe>();
}
