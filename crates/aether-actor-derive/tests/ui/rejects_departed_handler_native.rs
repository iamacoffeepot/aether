//! ADR-0079 §8: `Departed<W>` is the event of a wasm component's `ctx.watch`.
//! A native actor watches with `ctx.monitor` and takes `MonitorNotice`, so a
//! native handler over `Departed<W>` is refused with the form to write
//! instead, rather than failing as an unsatisfied `Kind` bound.

use aether_actor::{Departed, actor, protocol};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.departed_native.ping")]
struct Ping {
    seq: u32,
}

#[protocol]
trait Provider {
    fn ping(_: Ping);
}

struct NativeWatcher;

#[actor]
impl aether_substrate::actor::native::NativeActor for NativeWatcher {
    type Config = ();

    const NAMESPACE: &'static str = "native_watcher";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_actor::ActorInitError> {
        unimplemented!()
    }

    #[handler::event]
    fn on_gone(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _event: Departed<Provider>,
    ) {
    }
}

fn main() {}
