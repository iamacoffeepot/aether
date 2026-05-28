//! Test-fixture component for substrate-feature scenarios. Not a
//! demo, not exemplary — its only job is to expose substrate /
//! test-bench primitives (input subscription, drop, replace, capture)
//! to scenario assertions in a way that's easy to observe.
//!
//! Behaviour:
//!
//! - On every tick, sends `aether.test_fixture.tick_observed` to the
//!   test-bench observer mailbox (`aether.test_bench.observer`) with
//!   a monotonic counter. Lets scenarios count tick deliveries via
//!   `TestBench::count_observed` (issue 775 retired the
//!   `BroadcastCapability` MCP fan-out; the bench now owns a private
//!   catch-all observer mailbox for these scenario observations).
//! - On the first tick, emits a `tracing::info!("typed_send_alive")`
//!   that flows through the actor-aware subscriber (issue #581) →
//!   per-actor `LogBuffer` → drain at handler exit ships a `LogBatch`
//!   to the `aether.log` mailbox. Pre-#581 this fixture exercised the
//!   issue-563 stage-5 typed-sender path against `LogEvent`; #581
//!   demoted `LogEvent` to a non-mailable struct so the buffer-and-
//!   drain shape is the only sender path for log content.
//! - Receives `aether.test_fixture.set_render { r, g, b, visible }`
//!   to update render state. When `visible` is non-zero, `on_tick`
//!   emits a colored `DrawTriangle` to the chassis render sink, so
//!   `capture_frame` scenarios can observe pre-mail effects in the
//!   captured PNG.
//!
//! ADR-0090 c1: this fixture moved from `aether-test-fixture-probe`'s
//! `src/lib.rs` to `aether-test-fixtures/examples/probe.rs`. The
//! actor source is unchanged; the shared `TickObserved` / `SetRender`
//! kinds moved to the sibling lib so integration tests can import
//! them without reaching into a cdylib.

use aether_actor::{BootError, FfiActor, FfiCtx, MailSender, Resolver, actor};
use aether_capabilities::input::InputMailboxExt;
use aether_capabilities::{InputCapability, RenderCapability};
use aether_data::{Kind, MailboxId};
use aether_kinds::{DrawTriangle, Tick, Vertex};
use aether_test_fixtures::{SetRender, TEST_BENCH_OBSERVER_MAILBOX_NAME, TickObserved};

pub struct Probe {
    tick_count: u64,
    render: SetRender,
}

#[actor]
impl FfiActor for Probe {
    const NAMESPACE: &'static str = "test_fixture_probe";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(Probe {
            tick_count: 0,
            render: SetRender::default(),
        })
    }

    //noinspection DuplicatedCode
    /// Issue 640: explicit subscribe in `wire`; init is `Resolver`-only
    /// post-issue-703 and can't mail.
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        ctx.actor::<InputCapability>()
            .subscribe(Tick::ID, MailboxId(ctx.mailbox_id()));
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
    fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _: Tick) {
        self.tick_count += 1;
        ctx.send_to_named::<TickObserved>(
            TEST_BENCH_OBSERVER_MAILBOX_NAME,
            &TickObserved {
                count: self.tick_count,
            },
        );
        if self.tick_count == 1 {
            tracing::info!(target: "aether_test_fixture_probe", "typed_send_alive");
        }
        if self.render.visible != 0 {
            let r = f32::from(self.render.r) / 255.0;
            let g = f32::from(self.render.g) / 255.0;
            let b = f32::from(self.render.b) / 255.0;
            let v = |x: f32, y: f32| Vertex {
                x,
                y,
                z: 0.5,
                r,
                g,
                b,
            };
            ctx.actor::<RenderCapability>().send(&DrawTriangle {
                verts: [v(-0.9, -0.9), v(0.9, -0.9), v(0.0, 0.9)],
            });
        }
    }

    /// Updates the stored render state. Subsequent ticks paint the
    /// new color (or stop painting when `visible == 0`).
    ///
    /// # Agent
    /// Send via `send_mail` with `kind_name = "aether.test_fixture.set_render"`
    /// and params `{ r, g, b, visible }`. Used by `capture_frame`
    /// scenarios to flip the fixture's render output between frames.
    #[handler]
    fn on_set_render(&mut self, _ctx: &mut FfiCtx<'_>, mail: SetRender) {
        self.render = mail;
    }
}

aether_actor::export!(Probe);
