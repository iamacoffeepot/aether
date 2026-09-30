//! Issue 7109: a `test.republish.subject` successor that keeps every row of
//! the base module and adds an `InlineProbe` row, extending its contract.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{Bump, CountQuery, CountReport, InlineProbe, SubstrateHarnessObserver, TickObserved};

pub struct Subject {
    count: u32,
}

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for Subject {
    const NAMESPACE: &'static str = "test.republish.subject";

    type State = CountReport;

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Subject { count: 0 })
    }

    fn dehydrate(&self) -> CountReport {
        CountReport { count: self.count }
    }

    fn rehydrate(&mut self, CountReport { count }: CountReport) {
        self.count = count;
    }

    #[handler::tell]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: u64::from(self.count) });
    }

    #[handler::request]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }

    /// The added row: reports the count to the observer.
    #[handler::tell]
    fn on_inline_probe(&mut self, ctx: &mut WasmCtx<'_>, _probe: InlineProbe) {
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: u64::from(self.count) });
    }
}

aether_actor::export!(public = [Subject]);
