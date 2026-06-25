//! The runtime half — dispatcher + state. Compiled only under
//! `feature = "runtime"` (the gate `#[pull_up]` stamps onto the `mod runtime;`
//! declaration in `lib.rs`). The macro still *reads* this file at compile
//! time in every configuration, to harvest the handler kinds for the markers
//! it lifts up to `lib.rs`.
//!
//! Names referenced in handler signatures (`RenderCapability`, `Tick`,
//! `Resize`) are written *bare* so the type tokens the macro lifts upward
//! resolve in `lib.rs`'s scope too. `use super::*` brings them in here.

use super::{RenderCapability, Resize, Tick};
use pull_up_macro::handler;

/// Stand-in for the feature-gated, substrate-typed runtime state — the heavy
/// surface a transport-only / wasm build must never name.
pub struct RenderCapabilityState {
    pub frames: u64,
}

/// The dispatcher: `#[handler]`-tagged methods. The macro harvests the last
/// typed argument of each as the kind (`Tick`, `Resize`).
impl RenderCapability {
    #[handler]
    pub fn on_tick(state: &mut RenderCapabilityState, _mail: Tick) {
        state.frames += 1;
    }

    #[handler]
    pub fn on_resize(_state: &mut RenderCapabilityState, _mail: Resize) {}
}
