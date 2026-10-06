//! Issue #7463 (ADR-0247 rule 3): `wire` returns the birth's result, as
//! `init` does. A hook written without a return type is refused at the hook
//! with the signature to write, on both transports, rather than left to fail
//! as a type mismatch inside the generated forwarder.

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
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

struct Guest;

#[actor]
impl aether_actor::WasmActor for Guest {
    const NAMESPACE: &'static str = "wire_without_result_guest";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Guest)
    }

    fn wire(&mut self, _ctx: &mut aether_actor::WireCtx<'_, '_>) {}

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

#[allow(dead_code)]
struct Cap;

#[actor]
impl aether_substrate::actor::native::NativeActor for Cap {
    type Config = ();

    const NAMESPACE: &'static str = "wire_without_result_cap";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_substrate::BootError> {
        unimplemented!()
    }

    fn wire(&mut self, _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>) {}

    #[handler::tell]
    fn on_ping(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
    }
}

fn main() {}
