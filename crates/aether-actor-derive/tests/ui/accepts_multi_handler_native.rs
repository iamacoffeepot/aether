//! ADR-0134: a split `#[actor] impl NativeActor` may carry a
//! `#[handler::multi]` whose ctx is `NativeCtx<'_, Multi<K>>`. The
//! substrate-typed runtime impls cfg out in this fixture bin (no `runtime`
//! feature) — mirroring `accepts_actor_split_task_handler` — so the
//! assertion is that the macro accepts the multi signature (parses
//! `Multi<K>`, enforces `-> ()`) instead of erroring.

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
#[kind(name = "test.ping_multi")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.frame_multi")]
struct Frame {
    n: u32,
}

pub struct MultiCap;

#[allow(dead_code)]
struct MultiCapState {
    seen: u32,
}

#[actor(singleton)]
impl aether_substrate::actor::native::NativeActor for MultiCap {
    type State = MultiCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.multi_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<MultiCapState, aether_substrate::chassis::error::BootError> {
        Ok(MultiCapState { seen: 0 })
    }

    #[handler::multi]
    fn on_ping(
        state: &mut Self::State,
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_, aether_substrate::Multi<Frame>>,
        ping: Ping,
    ) {
        state.seen += 1;
        for n in 0..ping.seq {
            aether_substrate::Emit::emit(ctx, &Frame { n });
        }
    }
}

pub struct MultiSpawnCap;

#[allow(dead_code)]
struct MultiSpawnCapState {
    seen: u32,
}

// Issue 4158: a multi handler may also name the actor it dispatches for, to
// reach `spawn_child`. The reply mode is then the *first* ctx type argument
// and the actor the second, so a macro that reads the marker off the last
// argument sees `Self` here and rejects the signature.
#[actor(singleton)]
impl aether_substrate::actor::native::NativeActor for MultiSpawnCap {
    type State = MultiSpawnCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.multi_spawn_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<MultiSpawnCapState, aether_substrate::chassis::error::BootError> {
        Ok(MultiSpawnCapState { seen: 0 })
    }

    #[handler::multi]
    fn on_ping(
        state: &mut Self::State,
        ctx: &mut aether_substrate::actor::native::NativeCtx<'_, aether_substrate::Multi<Frame>, Self>,
        ping: Ping,
    ) {
        state.seen += 1;
        for n in 0..ping.seq {
            aether_substrate::Emit::emit(ctx, &Frame { n });
        }
    }
}

fn main() {}
