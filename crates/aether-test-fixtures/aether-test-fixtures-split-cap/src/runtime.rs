//! The runtime half — behaviour + state. Compiled only under
//! `feature = "runtime"` (the gate sits on the `mod runtime;` line in `lib.rs`).
//! `#[actor]` in `lib.rs` still reads this file in every configuration to lift
//! the identity, so the `NAMESPACE` const and the `#[handler]` kinds here drive
//! the always-on `Addressable` + `HandlesKind<K>` markers up there.
//!
//! Names in signatures (`SplitCap`, `Ping`, `Pong`) are written bare so the
//! kind tokens `#[actor]` lifts resolve in `lib.rs`'s scope too; `use super::*`
//! brings them in here.

use super::{Ping, Pong, SplitCap};
use aether_actor::runtime;
use aether_substrate::BootError;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};

/// The feature-gated, substrate-typed runtime state the identity boots into.
pub struct SplitCapState {
    pub pings: u32,
    pub pongs: u32,
}

/// The behaviour impl. `#[runtime]` keeps `type State` / `type Config` / `init`
/// on the gated runtime surface, moves the `#[handler]` bodies to an inherent
/// impl, and consumes `NAMESPACE` (lifted into `Addressable` by `#[actor]`).
#[runtime]
impl NativeActor for SplitCap {
    type State = SplitCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.split_cap";

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<SplitCapState, BootError> {
        Ok(SplitCapState { pings: 0, pongs: 0 })
    }

    #[handler]
    fn on_ping(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _ping: Ping) {
        state.pings += 1;
    }

    #[handler]
    fn on_pong(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _pong: Pong) {
        state.pongs += 1;
    }
}
