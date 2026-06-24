//! iamacoffeepot/aether#2338: a split `#[actor] impl NativeActor` (with
//! `type State = …`) may carry a `#[fallback]` whose first parameter is
//! `state: &mut Self::State` (the runtime state) rather than a `self` receiver,
//! mirroring the split `#[handler]` shape. Before this, the fallback validator
//! lacked the `is_split` branch and rejected the typed first param, blocking
//! every split cap with a catch-all (rpc server, wasm trampoline, http server).
//! The substrate-typed runtime impls cfg out in this fixture bin (no `runtime`
//! feature), so the marker surface is what compiles; the point is that the
//! macro accepts the split-fallback signature instead of erroring.

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
#[kind(name = "test.ping_fb")]
struct Ping {
    seq: u32,
}

pub struct FallbackCap;

#[allow(dead_code)]
struct FallbackCapState {
    seen: u32,
}

#[actor(singleton)]
impl aether_substrate::actor::native::NativeActor for FallbackCap {
    type State = FallbackCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.fallback_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<FallbackCapState, aether_substrate::chassis::error::BootError> {
        Ok(FallbackCapState { seen: 0 })
    }

    #[handler]
    fn on_ping(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
        state.seen += 1;
    }

    #[fallback]
    fn on_any(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _env: &aether_substrate::actor::native::envelope::Envelope,
    ) {
        state.seen += 1;
    }
}

fn main() {}
