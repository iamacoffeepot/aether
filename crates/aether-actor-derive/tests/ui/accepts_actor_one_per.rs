//! iamacoffeepot/aether#2330: a split `#[actor(instanced, one_per = "…")]` on
//! the native path compiles and registers `Cardinality::OnePer(entity)` in the
//! name inventory. The name-inventory `TemplateEntry` submission is gated only
//! on `not(target_family = "wasm")` (not on `runtime`), so it compiles in this
//! fixture bin and exercises the `OnePer` arm; the substrate-typed runtime impls
//! gate on the default `runtime` feature, absent here, so they cfg out.

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
#[kind(name = "test.ping_op")]
struct Ping {
    seq: u32,
}

pub struct PerThingCap;

#[allow(dead_code)]
struct PerThingCapState {
    seen: u32,
}

#[actor(instanced, one_per = "thing")]
impl aether_substrate::actor::native::NativeActor for PerThingCap {
    type State = PerThingCapState;
    type Config = ();

    const NAMESPACE: &'static str = "test.per_thing_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<PerThingCapState, aether_substrate::chassis::error::BootError> {
        Ok(PerThingCapState { seen: 0 })
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
