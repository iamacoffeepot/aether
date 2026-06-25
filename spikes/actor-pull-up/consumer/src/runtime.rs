//! The runtime half — behavior + state. Compiled only under `feature =
//! "runtime"` (the gate sits on the `mod runtime;` line in `lib.rs`).
//! `#[actor]` in `lib.rs` still *reads* this file in every configuration to
//! lift the identity, so the `NAMESPACE` const and the `#[handler]` kinds here
//! drive the always-on `Addressable` + `Handles<K>` impls up there.
//!
//! Names in signatures (`RenderCapability`, `Tick`, `Resize`) are written bare
//! so the kind tokens `#[actor]` lifts resolve in `lib.rs`'s scope too;
//! `use super::*` brings them in here.

use super::{RenderCapability, Resize, Runtime, Tick};
use pull_up_macro::{handler, runtime};

/// Stand-in for the feature-gated, substrate-typed runtime state.
pub struct RenderCapabilityState {
    pub frames: u64,
}

/// The behavior impl. `#[runtime]` keeps `type State` + `fn init` on the
/// `Runtime` trait impl, moves the `#[handler]` bodies to an inherent impl, and
/// consumes `NAMESPACE` (lifted into `Addressable` by `#[actor]`).
#[runtime]
impl Runtime for RenderCapability {
    const NAMESPACE: &str = "spike.render";
    type State = RenderCapabilityState;

    fn init() -> RenderCapabilityState {
        RenderCapabilityState { frames: 0 }
    }

    #[handler]
    fn on_tick(state: &mut RenderCapabilityState, _mail: Tick) {
        state.frames += 1;
    }

    #[handler]
    fn on_resize(_state: &mut RenderCapabilityState, _mail: Resize) {}
}
