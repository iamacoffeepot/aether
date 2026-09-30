//! Issue 7109: a `test.republish.subject` successor that keeps every row of
//! the base module and spawns a private inline child, `SubjectHelper`, from
//! `wire`. The child declares `depends(ClipboardCapability)`, which is
//! checked where a helper stands up (ADR-0241 §4): a republish from
//! `republish_subject_helper`, whose helper declares no dependency, is
//! refused for its live helper wherever no clipboard actor is live, while a
//! republish from the helperless base proceeds, since no helper is rebuilt.

use aether_actor::{ActorInitError, Subname, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_clipboard::ClipboardCapability;
use aether_test_fixtures_kinds::{Bump, CountQuery, CountReport, SubstrateHarnessObserver, TickObserved};

pub struct Subject {
    count: u32,
}

#[actor(root, spawns(SubjectHelper), depends(SubstrateHarnessObserver))]
impl WasmActor for Subject {
    const NAMESPACE: &'static str = "test.republish.subject";

    type State = CountReport;

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Subject { count: 0 })
    }

    /// Spawns the helper inline once wired.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        let _ = ctx.spawn_inline::<SubjectHelper>(Subname::Named("helper"), &());
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

/// The inline child whose declared dependency the host checks when it is
/// spawned or rebuilt. It counts the `Bump`s it receives.
pub struct SubjectHelper {
    bumps: u32,
}

#[actor(instanced, child_of(Subject), depends(ClipboardCapability))]
impl WasmActor for SubjectHelper {
    const NAMESPACE: &'static str = "test.republish.subject_helper";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(SubjectHelper { bumps: 0 })
    }

    #[handler::tell]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.bumps += 1;
    }
}

aether_actor::export!(public = [Subject], private = [SubjectHelper]);
