//! Actor-clock fixture (issue #7627): a guest that measures with `ctx.now()`.
//!
//! `ClockProbe` keeps one [`Instant`]: the reading it took at `init`, which a
//! [`ClockMark`] replaces with a fresh one. A [`ClockElapsed`] is answered
//! with how long ago that reading was taken, by a reading the handler takes
//! itself. A scenario built on a stepped clock reads from the answer whether
//! the guest's `init` and handler readings are the engine's.

use aether_actor::{ActorInitError, Instant, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{ClockElapsed, ClockElapsedReport, ClockMark};

pub struct ClockProbe {
    /// The reading taken at `init` and again at each [`ClockMark`].
    marked: Instant,
}

#[actor(root)]
impl WasmActor for ClockProbe {
    const NAMESPACE: &'static str = "test.clock_probe";

    fn init(ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { marked: ctx.now() })
    }

    #[handler::tell]
    fn on_mark(&mut self, ctx: &mut WasmCtx<'_>, _mark: ClockMark) {
        self.marked = ctx.now();
    }

    #[handler::request]
    fn on_elapsed(&mut self, ctx: &mut WasmCtx<'_>, _ask: ClockElapsed) -> ClockElapsedReport {
        ClockElapsedReport::of(ctx.now().since(self.marked))
    }
}
