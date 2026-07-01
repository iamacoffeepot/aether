//! ADR-0123 struct-hosted `#[actor]` happy path: `#[actor(singleton, rt_ok)]`
//! on a capability *struct* reads the sibling `rt_ok.rs` runtime module off disk,
//! selects its `impl NativeActor` (gap-1 trait filter), lifts the `NAMESPACE` +
//! the `on_ping` handler's `Ping` kind, and emits the always-on addressing
//! markers plus the gap-3 `include_bytes!` rebuild edge — all of which must
//! compile. The `Ping` kind the harvest lifts must resolve in this bin's scope.

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
#[kind(name = "test.ping_struct_hosted")]
struct Ping {
    seq: u32,
}

#[actor(singleton, rt_ok)]
pub struct Cap;

fn main() {}
