//! ADR-0227: handler signatures emit typed reply markers beside
//! `HandlesKind`: single replies use `Replies`, while multi replies use
//! `Streams` with the item kind read from `Multi<K>`.

use aether_actor::{Emit, Erased, Multi, Replies, Streams, WasmCtx, actor};

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

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.reply_marker.query")]
struct Query {
    count: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.reply_marker.row")]
struct Row {
    index: u32,
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

    #[handler::multi]
    fn on_query(&mut self, ctx: &mut WasmCtx<'_, Erased, Multi<Row>>, query: Query) {
        for index in 0..query.count {
            ctx.emit(&Row { index });
        }
    }
}

fn assert_replies<T: Replies<Ping, Reply = Pong>>() {}
fn assert_streams<T: Streams<Query, Item = Row>>() {}

fn main() {
    assert_replies::<ReplyProbe>();
    assert_streams::<ReplyProbe>();
}
