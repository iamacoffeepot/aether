//! Two `#[handler]` methods on one `#[actor] impl NativeActor` block
//! that accept the same mail kind are rejected, spanned at the later
//! handler — mirroring the wasm path so the diagnostic surface stays
//! symmetric across both expansions.

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
struct Dup;

#[actor]
impl aether_substrate::actor::native::NativeActor for Dup {
    type Config = ();

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_actor::BootError> {
        unimplemented!()
    }

    #[handler]
    fn on_first(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
    }

    #[handler]
    fn on_second(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
    }
}

fn main() {}
