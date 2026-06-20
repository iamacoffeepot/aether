//! An `#[actor] impl NativeActor` block that omits `const NAMESPACE` is
//! rejected at the type — rather than falling through to a later
//! "no associated const NAMESPACE" error against the surfaceless `Actor`
//! trait.

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

#[allow(dead_code)]
struct NoNamespace;

#[actor]
impl aether_substrate::actor::native::NativeActor for NoNamespace {
    type Config = ();

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_actor::ActorInitError> {
        unimplemented!()
    }

    #[handler]
    fn on_ping(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
    }
}

fn main() {}
