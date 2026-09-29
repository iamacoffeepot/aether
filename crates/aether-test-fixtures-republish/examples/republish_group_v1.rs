//! Issue 7109: the first version of the republish pair, which
//! `republish_group_v2` republishes.
//!
//! - `Gate` (`test.republish.gate`, instanced) answers `GateQuery` with the
//!   probes it recorded. This version has no `GateProbe` row, so it records
//!   none: a probe means something only to the guest a republish installs.
//! - `Peer` (`test.republish.peer`, root) counts `Bump`s, answers
//!   `CountQuery`, carries its count across a replace as `PeerState`, and
//!   reports `WireObserved` each time it is wired.
//!
//! The gate sends nothing from its lifecycle hooks, so every probe a test
//! sends it is the test's own.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, GateConfig, GateQuery, GateReport, PeerConfig, PeerState, SubstrateHarnessObserver,
    WireObserved,
};

pub struct Gate {
    seqs: Vec<u32>,
}

#[actor(instanced, root)]
impl WasmActor for Gate {
    type Config = GateConfig;
    const NAMESPACE: &'static str = "test.republish.gate";

    fn init(_config: GateConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Gate { seqs: Vec::new() })
    }

    #[handler::single]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: GateQuery) -> GateReport {
        GateReport { seqs: self.seqs.clone() }
    }
}

pub struct Peer {
    count: u32,
}

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for Peer {
    type Config = PeerConfig;
    const NAMESPACE: &'static str = "test.republish.peer";

    type State = PeerState;

    fn init(_config: PeerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Peer { count: 0 })
    }

    /// Report each run of the hook, so a test can count it.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        ctx.send::<SubstrateHarnessObserver>(&WireObserved);
    }

    fn dehydrate(&self) -> PeerState {
        PeerState { count: self.count }
    }

    fn rehydrate(&mut self, PeerState { count }: PeerState) {
        self.count = count;
    }

    #[handler::single]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
    }

    #[handler::single]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }
}

aether_actor::export!(public = [Gate, Peer]);
