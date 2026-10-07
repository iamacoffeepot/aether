//! Issue 7109: the second version of the republish pair, republishing
//! `republish_group_v1`.
//!
//! - `ProbeGate` (the crate's shared type, issue 7143) adds a `GateProbe`
//!   row: it records each probe's `seq` in arrival order, and `GateQuery`
//!   reads the order back. A test that holds a member prepared sends the
//!   probes itself, and the order the winning guest reports is the order the
//!   gate released them in.
//! - `Peer` keeps v1's rows and state. With `PeerConfig::trap_on_rehydrate`
//!   set, its `on_rehydrate` reports `TickObserved`, which a failed candidate
//!   must never deliver, and traps, so a republish that carries the peer's
//!   state fails at rehydrate.

use std::process;

use aether_actor::{ActorInitError, PriorState, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, WireCtx, actor};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, PeerConfig, PeerState, SubstrateHarnessObserver, TickObserved, WireObserved,
};
use aether_test_fixtures_republish::ProbeGate;

pub struct Peer {
    count: u32,
    trap_on_rehydrate: bool,
}

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for Peer {
    type Config = PeerConfig;
    const NAMESPACE: &'static str = "test.republish.peer";

    fn init(config: PeerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Peer { count: 0, trap_on_rehydrate: config.trap_on_rehydrate })
    }

    /// Report each run of the hook, so a test can count it.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.send::<SubstrateHarnessObserver>(&WireObserved);
        Ok(())
    }

    /// Hand-written, where v1 generates it from `type State`, because the
    /// rehydrate side must be able to trap.
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
        ctx.save_state_kind::<PeerState>(0, &PeerState { count: self.count })
    }

    /// Restore v1's count, or, with `trap_on_rehydrate` set, report
    /// `TickObserved` and trap: `abort` lowers to `unreachable`, which the
    /// host reports as an `on_rehydrate` failure.
    fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) -> Result<(), ActorInitError> {
        if self.trap_on_rehydrate {
            ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: u64::from(self.count) });
            process::abort();
        }
        if let Some(saved) = prior.decode_kind::<PeerState>() {
            self.count = saved.count;
        }
        Ok(())
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    #[handler::request]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }
}

aether_actor::export!(public = [ProbeGate, Peer]);
