//! Parent-relative component-peer routing fixture (issue #4535).
//!
//! `ParentPeerCaller` receives `Bump` and forwards it through the bare-type
//! `ctx.actor::<ParentPeerTarget>()`. The target emits the existing
//! `TickObserved` marker to the substrate-harness observer. A harness scenario
//! can therefore load both actors beneath an explicit logical parent and
//! observe whether the caller selected the target from that same parent scope.

#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{Bump, SubstrateHarnessObserver, TickObserved};

pub struct ParentPeerCaller;

#[actor(depends(ParentPeerTarget))]
impl WasmActor for ParentPeerCaller {
    const NAMESPACE: &'static str = "test.parent_peer.caller";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ParentPeerCaller)
    }

    #[handler::single]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        ctx.actor::<ParentPeerTarget>().send(&Bump);
    }
}

pub struct ParentPeerTarget;

#[actor(depends(SubstrateHarnessObserver))]
impl WasmActor for ParentPeerTarget {
    const NAMESPACE: &'static str = "test.parent_peer.target";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ParentPeerTarget)
    }

    #[handler::single]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        ctx.actor::<SubstrateHarnessObserver>().send(&TickObserved { count: 1 });
    }
}
