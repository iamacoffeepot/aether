//! Issue 7109: a `test.republish.subject` successor that keeps every row of
//! the base module and adds a boot actor, `SubjectBoot` (ADR-0147), where the
//! base module has none.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_test_fixtures_kinds::{BootObserved, Bump, CountQuery, CountReport, SubstrateHarnessObserver, TickObserved};

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
}

/// The added boot actor. It reports a `BootObserved` once wired, and counts
/// the `Bump`s it receives.
pub struct SubjectBoot {
    bumps: u32,
}

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for SubjectBoot {
    const NAMESPACE: &'static str = "test.republish.subject_boot";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(SubjectBoot { bumps: 0 })
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.send::<SubstrateHarnessObserver>(&BootObserved { marker: 0 });
        Ok(())
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.bumps += 1;
    }
}

aether_actor::export!(boot = SubjectBoot, public = [Subject]);
