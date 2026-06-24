//! iamacoffeepot/aether#2330: `one_per` declares the entity relationship of an
//! *instanced* family — on a singleton (or a bare `#[actor]`) it is meaningless,
//! so the macro rejects it, mirroring the `#[bridge]` guard. The diagnostic
//! fires before path resolution, so it does not depend on linking the substrate.

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
#[kind(name = "test.ping_opwi")]
struct Ping {
    seq: u32,
}

pub struct BadCap;

#[allow(dead_code)]
struct BadCapState {
    seen: u32,
}

#[actor(singleton, one_per = "thing")]
impl aether_substrate::actor::native::NativeActor for BadCap {
    type State = BadCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.bad_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<BadCapState, aether_substrate::chassis::error::BootError> {
        Ok(BadCapState { seen: 0 })
    }

    #[handler]
    fn on_ping(
        state: &mut Self::State,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
        state.seen += 1;
    }
}

fn main() {}
