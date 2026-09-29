//! Two `#[handler(task)]` methods with the same `TaskDone<O, C>` pair are
//! refused: completions route by that pair, so the first-tried handler would
//! shadow the second. `accepts_task_handlers_sharing_output_with_distinct_contexts`
//! is the counter-case.

use aether_actor::actor;

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.ping_task")]
pub struct Ping {
    seq: u32,
}

struct Reply;

struct First;

pub struct TaskCap;

struct TaskCapState {
    seen: u32,
}

#[actor(singleton)]
impl aether_substrate::actor::native::NativeActor for TaskCap {
    type State = TaskCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.task_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<TaskCapState, aether_substrate::chassis::error::BootError> {
        Ok(TaskCapState { seen: 0 })
    }

    #[handler::single]
    fn on_ping(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
        state.seen = state.seen.wrapping_add(1);
    }

    #[handler(task)]
    fn on_first_done(
        state: &mut Self::State,
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        done: aether_substrate::actor::native::TaskDone<Reply, First>,
    ) {
        done.resolve(ctx);
        state.seen = state.seen.wrapping_add(1);
    }

    #[handler(task)]
    fn on_second_done(
        state: &mut Self::State,
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        done: aether_substrate::actor::native::TaskDone<Reply, First>,
    ) {
        done.resolve(ctx);
        state.seen = state.seen.wrapping_add(1);
    }
}

fn main() {
    let _ = (Reply, First);
}
