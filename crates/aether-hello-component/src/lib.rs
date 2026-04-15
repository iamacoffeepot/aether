// First real aether component, now written against the ADR-0014
// `Component` trait. On each tick it emits a fixed clip-space
// triangle to the substrate's render sink.
//
// What the Component trait + export! macro replace compared to the
// ADR-0012 shape this file used to carry:
//   - hand-written `#[unsafe(no_mangle)] pub unsafe extern "C" fn
//     init/receive` → `export!(Hello)` emits the shims.
//   - `static mut Option<KindId<Tick>>` / `static mut Option<Sink<...>>`
//     → fields on `Hello`, populated during `init`.
//   - `unsafe` blocks around every access to the statics → none;
//     `&mut self` in `receive` is ordinary safe Rust.
//
// Behavioral parity: resolves Tick and the "render" sink at init,
// sends the triangle whenever a tick arrives. Nothing else.

use aether_component::{Component, Ctx, InitCtx, KindId, Mail, Sink};
use aether_substrate_mail::{DrawTriangle, Tick, Vertex};

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
    render: Sink<DrawTriangle>,
}

impl Component for Hello {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Hello {
            tick: ctx.resolve::<Tick>(),
            render: ctx.resolve_sink::<DrawTriangle>("render"),
        }
    }

    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
        if self.tick.matches(mail.kind()) {
            ctx.send(&self.render, &TRIANGLE);
        }
    }
}

aether_component::export!(Hello);
