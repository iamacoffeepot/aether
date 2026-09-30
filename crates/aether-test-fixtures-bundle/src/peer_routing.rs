//! Component-peer routing fixture (issue #4535, ADR-0241 §5).
//!
//! `ParentPeerCaller` receives `Bump` and forwards it through
//! `ctx.send::<ParentPeerTarget>`, which resolves the target by type as a
//! root singleton. The target emits the existing `TickObserved` marker to the
//! substrate-harness observer. A harness scenario can therefore load both
//! actors and observe whether the caller reached the target at its published
//! name.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{Bump, SubstrateHarnessObserver, TickObserved};

pub struct ParentPeerCaller;

#[actor(root, depends(ParentPeerTarget))]
impl WasmActor for ParentPeerCaller {
    const NAMESPACE: &'static str = "test.parent_peer.caller";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ParentPeerCaller)
    }

    #[handler::tell]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        ctx.send::<ParentPeerTarget>(&Bump);
    }
}

pub struct ParentPeerTarget;

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for ParentPeerTarget {
    const NAMESPACE: &'static str = "test.parent_peer.target";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ParentPeerTarget)
    }

    #[handler::tell]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: 1 });
    }
}

/// An instanced stand-in for [`ParentPeerTarget`]: it answers `Bump` as the
/// target does, but under a key its load names, so a test can host a
/// target-shaped actor beside the singleton target itself.
pub struct ParentPeerStandIn;

#[actor(instanced, root, depends(SubstrateHarnessObserver))]
impl WasmActor for ParentPeerStandIn {
    const NAMESPACE: &'static str = "test.parent_peer.stand_in";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ParentPeerStandIn)
    }

    #[handler::tell]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: 1 });
    }
}
