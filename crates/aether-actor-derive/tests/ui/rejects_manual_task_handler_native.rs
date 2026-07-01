//! Issue #2460: a `#[handler(task)]` always dispatches with the single
//! reply class — the completion reply rides `TaskDone`, not the handler
//! class — so a non-`Single` marker like `#[handler::manual(task)]` would
//! be silently discarded. The macro rejects it at the boundary instead of
//! dropping the designation without a diagnostic.

use aether_actor::actor;

#[allow(dead_code)]
struct Reply {
    value: u32,
}

pub struct TaskCap;

#[actor]
impl aether_substrate::actor::native::NativeActor for TaskCap {
    type Config = ();

    const NAMESPACE: &'static str = "test.manual_task_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_actor::ActorInitError> {
        Ok(TaskCap)
    }

    #[handler::manual(task)]
    fn on_done(
        &mut self,
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        done: aether_substrate::actor::native::TaskDone<Reply>,
    ) {
        done.resolve(ctx);
    }
}

fn main() {}
