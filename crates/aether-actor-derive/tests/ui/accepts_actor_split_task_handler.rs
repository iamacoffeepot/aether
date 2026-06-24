//! iamacoffeepot/aether#2341: a split `#[actor] impl NativeActor` (with
//! `type State = …`) may carry a `#[handler(task)]` (ADR-0093 deferred
//! completion) whose first parameter is `state: &mut Self::State` rather than a
//! `self` receiver, mirroring the split `#[handler]` / `#[fallback]` shapes.
//! Before this, `extract_task_handler_types` lacked the `is_split` branch and
//! rejected the typed first param, blocking every split cap with a task handler
//! (rpc deferred-echo, gemini/anthropic). The substrate-typed runtime impls cfg
//! out in this fixture bin (no `runtime` feature), so the assertion is that the
//! macro accepts the split task-handler signature instead of erroring.

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
struct Ping {
    seq: u32,
}

#[allow(dead_code)]
struct Reply {
    value: u32,
}

pub struct TaskCap;

#[allow(dead_code)]
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

    #[handler]
    fn on_ping(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
        state.seen += 1;
    }

    #[handler(task)]
    fn on_ping_done(
        state: &mut Self::State,
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        done: aether_substrate::actor::native::TaskDone<Reply>,
    ) {
        done.resolve(ctx);
        state.seen += 1;
    }
}

fn main() {}
