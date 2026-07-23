//! `intercept_slider_v2` — the swap target for the state-carry scenario
//! (issue 2688).
//!
//! Same `{ cap, count }` state schema as `intercept_slider`, so a live swap
//! offers the old script's blob to this one's `state_load`. It surfaces the
//! *carried* `count` on every committed change (not only on a clamp), so the
//! scenario sees the count continue rather than reset across the swap. Its
//! `on_attach` seeds a deliberately *different* default cap: if the swap
//! failed to carry `cap`, this script would clamp to `1000.0` (i.e. not at
//! all for the driven range) rather than the carried `20.0`, making the
//! carried-cap clamp a genuine tripwire.

// See `intercept_slider.rs`: the `#[behavior]` dispatch is wasm-only, so the
// handlers read as dead on the host build.
#![allow(dead_code)]

use aether_behavior::BehaviorCtx;
use aether_behavior_derive::behavior;
use aether_test_fixtures_behavior::{RadioSelected, SliderChanged};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct InterceptSliderV2 {
    cap: f32,
    count: u32,
}

#[behavior]
impl Behavior for InterceptSliderV2 {
    /// Seed a distinct default cap so a lost carry is observable — a carried
    /// `cap` (non-zero) leaves this untouched.
    #[on_attach]
    fn attached(&mut self, _ctx: &mut BehaviorCtx) {
        if self.cap == 0.0 {
            self.cap = 1000.0;
        }
    }

    #[on]
    fn on_change(&mut self, ctx: &mut BehaviorCtx, m: &mut SliderChanged) {
        if !m.committed {
            ctx.consume();
            return;
        }
        if m.value > self.cap {
            m.value = self.cap;
            self.count += 1;
        }
        // Surface the carried count on every committed change so the swap's
        // state carry is observable even when the value does not clamp.
        ctx.panel().emit(&RadioSelected { index: self.count });
    }
}
