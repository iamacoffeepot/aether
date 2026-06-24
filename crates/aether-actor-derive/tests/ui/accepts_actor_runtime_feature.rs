//! iamacoffeepot/aether#2330: a split `#[actor(singleton, runtime_feature =
//! "…")]` on the native path compiles — the macro accepts the arg and gates the
//! runtime impls (`Lifecycle` / `Dispatch` / `NativeActor`) on the named feature
//! instead of the default `runtime`. In this fixture bin the named feature is
//! absent, so those substrate-typed impls cfg out; the always-on `Addressable` /
//! `HandlesKind` markers and the name-inventory entry are what remain and must
//! compile.

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
#[kind(name = "test.ping_rf")]
struct Ping {
    seq: u32,
}

pub struct GatedCap;

#[allow(dead_code)]
struct GatedCapState {
    seen: u32,
}

#[actor(singleton, runtime_feature = "gated-native")]
impl aether_substrate::actor::native::NativeActor for GatedCap {
    type State = GatedCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.gated_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<GatedCapState, aether_substrate::chassis::error::BootError> {
        Ok(GatedCapState { seen: 0 })
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
