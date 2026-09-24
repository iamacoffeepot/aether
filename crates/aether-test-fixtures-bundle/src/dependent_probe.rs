//! Declared-dependency load fixture (issue #6277).
//!
//! `DependentProbe` declares `depends(ParentPeerTarget)`: it may only load
//! once the target holds a `Live` route beneath the same parent. It counts
//! the `Bump` mails it receives and reports the count, so the probe stays
//! observable like every other peer-routing fixture.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{Bump, SubstrateHarnessObserver, TickObserved};

use super::peer_routing::ParentPeerTarget;

pub struct DependentProbe {
    bumps: u64,
}

#[actor(depends(ParentPeerTarget, SubstrateHarnessObserver))]
impl WasmActor for DependentProbe {
    const NAMESPACE: &'static str = "test.parent_peer.dependent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(DependentProbe { bumps: 0 })
    }

    #[handler::single]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.bumps += 1;
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: self.bumps });
    }
}
