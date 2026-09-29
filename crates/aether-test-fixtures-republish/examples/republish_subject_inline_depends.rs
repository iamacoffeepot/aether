//! Issue 7109: a `test.republish.subject` successor that keeps every row of
//! the base module and spawns a private inline child, `SubjectHelper`, from
//! `wire`. The child declares `depends(ClipboardCapability)`, so a republish
//! to this module is refused wherever no clipboard actor is live.

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

    #[handler::single]
    fn on_bump(&mut self, ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.count += 1;
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: u64::from(self.count) });
    }

    #[handler::single]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.count }
    }
}

/// The inline child whose declared dependency the host checks when the module
/// loads. It counts the `Bump`s it receives.
pub struct SubjectHelper {
    bumps: u32,
}

#[actor(instanced, composable, depends(ClipboardCapability))]
impl WasmActor for SubjectHelper {
    const NAMESPACE: &'static str = "test.republish.subject_helper";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(SubjectHelper { bumps: 0 })
    }

    #[handler::single]
    fn on_bump(&mut self, _ctx: &mut WasmCtx<'_>, _bump: Bump) {
        self.bumps += 1;
    }
}

aether_actor::export!(public = [Subject], private = [SubjectHelper]);
