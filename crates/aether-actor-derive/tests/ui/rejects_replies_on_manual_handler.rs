//! ADR-0227: a manual handler can issue arbitrary replies and therefore emits
//! `HandlesKind<K>` but no `Replies<K>` marker.

use aether_actor::{Erased, Manual, Replies, WasmCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.replies.manual.ping")]
struct Ping {
    seq: u32,
}

struct ManualProbe;

#[actor]
impl aether_actor::WasmActor for ManualProbe {
    const NAMESPACE: &'static str = "manual_reply_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Self)
    }

    #[handler::manual]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_, Erased, Manual>, _ping: Ping) {}
}

fn assert_replies<T: Replies<Ping>>() {}

fn main() {
    assert_replies::<ManualProbe>();
}
