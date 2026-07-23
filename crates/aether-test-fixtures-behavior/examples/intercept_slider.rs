//! `intercept_slider` — the realistic main fixture behavior (issue 2688).
//!
//! Wraps a slider. It consumes uncommitted drag noise, clamps a committed
//! value to an authored `cap`, and on a clamp increments an authored `count`
//! and emits it up the panel lane (as a `RadioSelected`, the panel's numeric
//! value-up log line) so the effect drain is observable. The `{ cap, count }`
//! struct's serde fields *are* the persisted state the swap carries.

// The `#[behavior]` dispatch that calls these handlers is emitted only on
// wasm (the guest `filter` export); the host build (workspace clippy
// `--all-targets`) compiles the script but never calls it, so the handlers
// read as dead there.
#![allow(dead_code)]

use aether_behavior::BehaviorCtx;
use aether_behavior_derive::behavior;
use aether_test_fixtures_behavior::{RadioSelected, SliderChanged};
use serde::{Deserialize, Serialize};

/// Authored state: the clamp `cap` and how many committed values it clamped.
/// The default `state_save` / `state_load` serialize the whole struct, so a
/// swap carries both fields forward.
#[derive(Default, Serialize, Deserialize)]
struct InterceptSlider {
    cap: f32,
    count: u32,
}

#[behavior]
impl Behavior for InterceptSlider {
    /// Seed the clamp cap once, post-restore.
    #[on_attach]
    fn attached(&mut self, _ctx: &mut BehaviorCtx) {
        if self.cap == 0.0 {
            self.cap = 20.0;
        }
    }

    /// Intercept the slider's value-up. `&mut` is the intercept intent: the
    /// mutated event re-encodes and forwards, a consumed one does not.
    #[on]
    fn on_change(&mut self, ctx: &mut BehaviorCtx, m: &mut SliderChanged) {
        if !m.committed {
            // Drop the uncommitted drag stream — it never forwards up-lane.
            ctx.consume();
            return;
        }
        if m.value > self.cap {
            m.value = self.cap;
            self.count += 1;
            // The clamp effect: surface the running count up the panel lane.
            ctx.panel().emit(&RadioSelected { index: self.count });
        }
    }
}
