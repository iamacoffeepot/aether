// First real aether component. On each tick it emits a fixed
// clip-space triangle to the substrate's render sink. It also
// answers ADR-0013 `aether.ping` mail with a matching `aether.pong`
// back to the originating Claude session — a minimal round-trip
// smoke test proving reply-to-sender works end-to-end over the hub.

use aether_component::{Component, Ctx, InitCtx, KindId, Mail, Sink};
use aether_kinds::{DrawTriangle, Ping, Pong, Tick, Vertex};

static TRIANGLE: DrawTriangle = DrawTriangle {
    verts: [
        Vertex {
            x: 0.0,
            y: 0.5,
            r: 1.0,
            g: 0.0,
            b: 0.0,
        },
        Vertex {
            x: -0.5,
            y: -0.5,
            r: 0.0,
            g: 1.0,
            b: 0.0,
        },
        Vertex {
            x: 0.5,
            y: -0.5,
            r: 0.0,
            g: 0.0,
            b: 1.0,
        },
    ],
};

pub struct Hello {
    tick: KindId<Tick>,
    ping: KindId<Ping>,
    pong: KindId<Pong>,
    render: Sink<DrawTriangle>,
}

impl Component for Hello {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Hello {
            tick: ctx.resolve::<Tick>(),
            ping: ctx.resolve::<Ping>(),
            pong: ctx.resolve::<Pong>(),
            render: ctx.resolve_sink::<DrawTriangle>("render"),
        }
    }

    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
        if self.tick.matches(mail.kind()) {
            ctx.send(&self.render, &TRIANGLE);
        } else if let Some(ping) = mail.decode(self.ping)
            && let Some(sender) = mail.sender()
        {
            // Echo the sequence number so the caller can pair request
            // and reply when multiple pings are in flight. No sender
            // (component-origin or broadcast) silently drops the ping
            // — there's nothing to reply to.
            ctx.reply(sender, self.pong, &Pong { seq: ping.seq });
        }
    }
}

aether_component::export!(Hello);
