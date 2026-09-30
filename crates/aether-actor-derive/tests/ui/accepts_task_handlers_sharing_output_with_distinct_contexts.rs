//! Two `#[handler(task)]` methods may share the `TaskDone` output type when
//! their context types differ: completions route by the `(O, C)` pair the
//! ledger's `try_take::<O, C>` probe matches, so the pair, not `O` alone, is
//! what must be distinct. `rejects_duplicate_task_handler_pair` is the
//! counter-case: the same pair twice is still refused.

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

struct Second;

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

    #[handler::tell]
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
        done: aether_substrate::actor::native::TaskDone<Reply, Second>,
    ) {
        done.resolve(ctx);
        state.seen = state.seen.wrapping_add(1);
    }
}

fn main() {
    let _ = (Reply, First, Second);
}
