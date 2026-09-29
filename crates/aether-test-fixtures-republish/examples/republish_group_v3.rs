//! Issue 7086: a version of the republish pair whose gate changes its config
//! kind, republishing `republish_group_v1`.
//!
//! - `Gate` keeps v1's rows but is built with a `GateLabelledConfig`, a kind
//!   v1's gate does not declare, so every live gate needs a config when this
//!   version republishes v1. It answers `GateQuery` with its config's label,
//!   so a test reads which config each instance was built with.
//! - `Peer` is v1's, unchanged.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, GateLabelledConfig, GateQuery, GateQueryResult, PeerConfig, PeerState,
    SubstrateHarnessObserver, WireCountQuery, WireObserved,
};

pub struct Gate {
    label: u32,
    wired: u32,
}

#[actor(instanced, root)]
impl WasmActor for Gate {
    type Config = GateLabelledConfig;
    const NAMESPACE: &'static str = "test.republish.gate";

    fn init(config: GateLabelledConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Gate { label: config.label, wired: 0 })
    }

    fn wire(&mut self, _ctx: &mut WireCtx<'_, '_>) {
        self.wired += 1;
    }

    #[handler::single]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: GateQuery) -> GateQueryResult {
        GateQueryResult { seqs: vec![self.label] }
    }

    #[handler::single]
    fn on_wired(&mut self, _ctx: &mut WasmCtx<'_>, _query: WireCountQuery) -> CountReport {
        CountReport { count: self.wired }
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
