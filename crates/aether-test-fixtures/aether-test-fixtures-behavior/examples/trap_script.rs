//! `trap_script` — a deliberately-trapping fixture behavior (issue 2688).
//!
//! Its one intercept spins to fuel exhaustion, so every filter call traps.
//! The host must fail open: the in-flight `SliderChanged` forwards
//! untransformed and the wrapped slider keeps working, rather than the trap
//! wedging the lane. After `disable_after_traps` consecutive faults the host
//! drops the script to passthrough.

// See `intercept_slider.rs`: the `#[behavior]` dispatch is wasm-only, so the
// handler reads as dead on the host build.
#![allow(dead_code)]
// The `#[on]` handler ABI takes `&mut self`; this trap ignores it.
#![allow(clippy::unused_self)]

use std::hint::black_box;

use aether_behavior::BehaviorCtx;
use aether_behavior_derive::behavior;
use aether_test_fixtures_behavior::SliderChanged;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct TrapScript;

#[behavior]
impl Behavior for TrapScript {
    #[on]
    fn on_change(&mut self, _ctx: &mut BehaviorCtx, _m: &mut SliderChanged) {
        // Burn fuel until the host's per-call budget traps this call.
        let mut spin: u64 = 0;
        loop {
            spin = spin.wrapping_add(1);
            black_box(spin);
        }
    }
}
