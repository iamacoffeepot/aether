//! Replace contract fixtures (ADR-0231 §5, issue #6444).
//!
//! `ContractBase` handles `Bump` silently, raising `TickObserved` at the
//! substrate-harness observer, and answers `CountQuery` with `CountReport`.
//! Each other type is a candidate replacement for it: `ContractDropped` lacks
//! the `CountQuery` row, `ContractChanged` answers it with nothing, and
//! `ContractExtended` keeps both rows and adds a silent `InlineProbe`.

#![allow(clippy::unused_self)] // aether-suppression-request: the ADR-0033 dispatch ABI fixes the handler signature at `&mut self`, and these fixtures are stateless — the same allow `peer_routing` carries

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{Bump, CountQuery, CountReport, InlineProbe, SubstrateHarnessObserver, TickObserved};

pub struct ContractBase;

#[actor(depends(SubstrateHarnessObserver))]
impl WasmActor for ContractBase {
    const NAMESPACE: &'static str = "test.contract.base";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractBase)
    }

    #[handler::single]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: 1 });
    }

    #[handler::single]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: 0 }
    }
}

pub struct ContractDropped;

#[actor]
impl WasmActor for ContractDropped {
    const NAMESPACE: &'static str = "test.contract.dropped";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractDropped)
    }

    #[handler::single]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {}
}

pub struct ContractChanged;

#[actor]
impl WasmActor for ContractChanged {
    const NAMESPACE: &'static str = "test.contract.changed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractChanged)
    }

    #[handler::single]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {}

    #[handler::single]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) {}
}

pub struct ContractExtended;

#[actor]
impl WasmActor for ContractExtended {
    const NAMESPACE: &'static str = "test.contract.extended";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractExtended)
    }

    #[handler::single]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {}

    #[handler::single]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: 0 }
    }

    #[handler::single]
    fn on_inline_probe(&mut self, _ctx: &mut WasmCtx<'_>, _probe: InlineProbe) {}
}
