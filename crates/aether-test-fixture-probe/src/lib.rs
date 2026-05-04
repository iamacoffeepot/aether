//! Test-fixture component for substrate-feature scenarios. Not a
//! demo, not exemplary — its only job is to expose substrate /
//! test-bench primitives (input subscription, drop, replace, capture)
//! to scenario assertions in a way that's easy to observe.
//!
//! Behaviour:
//!
//! - On every tick, broadcasts `aether.test_fixture.tick_observed`
//!   with a monotonic counter. Lets scenarios count tick deliveries
//!   via `TestBench::count_observed` (broadcasts arrive on the
//!   loopback and get recorded by kind name).
//! - Receives `aether.test_fixture.set_render { r, g, b, visible }`
//!   to update render state. When `visible` is non-zero, on_tick
//!   emits a colored `DrawTriangle` to the chassis render sink, so
//!   `capture_frame` scenarios can observe pre-mail effects in the
//!   captured PNG.

use aether_component::{BootError, Component, Ctx, InitCtx, Mailbox, handlers, resolve_mailbox};
use aether_kinds::{DrawTriangle, Tick, Vertex};
use bytemuck::{Pod, Zeroable};

/// Broadcast payload emitted on each tick. Postcard-shaped — schema
/// rides in the wasm's `aether.kinds` custom section, so the bench's
/// loopback decoder can record the kind name without the test
/// pre-registering anything.
#[derive(
    aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.test_fixture.tick_observed")]
pub struct TickObserved {
    pub count: u64,
}

/// Driver kind: scenarios send this to flip the fixture's render
/// state. `visible == 0` halts the per-tick draw; any other value
/// enables it. Cast-shape so encoding is just a memcpy of four
/// bytes — keeps the test-side `MailEnvelope.payload` construction
/// trivial.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.test_fixture.set_render")]
pub struct SetRender {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub visible: u8,
}

const BROADCAST: Mailbox<TickObserved> = resolve_mailbox::<TickObserved>("hub.claude.broadcast");
const RENDER: Mailbox<DrawTriangle> = resolve_mailbox::<DrawTriangle>("aether.render");

pub struct Probe {
    tick_count: u64,
    render: SetRender,
}

#[handlers]
impl Component for Probe {
    const NAMESPACE: &'static str = "test_fixture_probe";

    fn init(_ctx: &mut InitCtx<'_>) -> Result<Self, BootError> {
        Ok(Probe {
            tick_count: 0,
            render: SetRender::default(),
        })
    }

    /// Counts ticks delivered to this mailbox; broadcasts the running
    /// total so scenarios can observe it on the loopback. When the
    /// stored render state is `visible`, also emits a colored
    /// `DrawTriangle` covering most of the frame so `capture_frame`
    /// scenarios can see the pre-mail effect in the PNG.
    ///
    /// # Agent
    /// Not sent manually; the substrate's tick fanout fires it once
    /// per advance for every input-subscribed mailbox. Watch
    /// `receive_mail` for `aether.test_fixture.tick_observed` to see
    /// the count climbing.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _: Tick) {
        self.tick_count += 1;
        BROADCAST.send(
            ctx.transport(),
            &TickObserved {
                count: self.tick_count,
            },
        );
        if self.render.visible != 0 {
            let r = self.render.r as f32 / 255.0;
            let g = self.render.g as f32 / 255.0;
            let b = self.render.b as f32 / 255.0;
            let v = |x: f32, y: f32| Vertex {
                x,
                y,
                z: 0.5,
                r,
                g,
                b,
            };
            RENDER.send(
                ctx.transport(),
                &DrawTriangle {
                    verts: [v(-0.9, -0.9), v(0.9, -0.9), v(0.0, 0.9)],
                },
            );
        }
    }

    /// Updates the stored render state. Subsequent ticks paint the
    /// new color (or stop painting when `visible == 0`).
    ///
    /// # Agent
    /// Send via `send_mail` with `kind_name = "aether.test_fixture.set_render"`
    /// and params `{ r, g, b, visible }`. Used by capture_frame
    /// scenarios to flip the fixture's render output between frames.
    #[handler]
    fn on_set_render(&mut self, _ctx: &mut Ctx<'_>, mail: SetRender) {
        self.render = mail;
    }
}

aether_component::export!(Probe);
