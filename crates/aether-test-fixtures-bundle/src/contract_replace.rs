//! Replace contract fixtures (ADR-0231 §5, issue #6444).
//!
//! `ContractBase` handles `Bump` silently, raising `TickObserved` at the
//! substrate-harness observer, and answers `CountQuery` with `CountReport`.
//! Each other type is a candidate replacement for it: `ContractDropped` lacks
//! the `CountQuery` row, `ContractChanged` answers it with nothing,
//! `ContractExtended` keeps both rows and adds a silent `InlineProbe`, and
//! `ContractFallback` keeps both rows, bumps as `ContractBase` does, and adds
//! a `#[fallback]`.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{Bump, CountQuery, CountReport, InlineProbe, SubstrateHarnessObserver, TickObserved};

pub struct ContractBase;

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for ContractBase {
    const NAMESPACE: &'static str = "test.contract.base";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractBase)
    }

    #[handler::tell]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: 1 });
    }

    #[handler::request]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: 0 }
    }
}

pub struct ContractDropped;

#[actor(root)]
impl WasmActor for ContractDropped {
    const NAMESPACE: &'static str = "test.contract.dropped";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractDropped)
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {}
}

pub struct ContractChanged;

#[actor(root)]
impl WasmActor for ContractChanged {
    const NAMESPACE: &'static str = "test.contract.changed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractChanged)
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {}

    #[handler::tell]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) {}
}

pub struct ContractExtended;

#[actor(root)]
impl WasmActor for ContractExtended {
    const NAMESPACE: &'static str = "test.contract.extended";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractExtended)
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {}

    #[handler::request]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: 0 }
    }

    #[handler::tell]
    fn on_inline_probe(&mut self, _ctx: &mut WasmCtx<'_>, _probe: InlineProbe) {}
}

pub struct ContractFallback;

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for ContractFallback {
    const NAMESPACE: &'static str = "test.contract.fallback";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ContractFallback)
    }

    #[handler::tell]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: 1 });
    }

    #[handler::request]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: 0 }
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}
