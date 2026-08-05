//! ADR-0123 struct-hosted `#[actor]` happy path: `#[actor(instanced, rt_ok)]`
//! on a capability *struct* reads the sibling `rt_ok.rs` runtime module off disk,
//! selects its `impl NativeActor` (gap-1 trait filter), lifts the `NAMESPACE` +
//! the `on_ping` handler's `Ping` kind, and emits the always-on addressing
//! markers plus the gap-3 `include_bytes!` rebuild edge — all of which must
//! compile. The `Ping` kind the harvest lifts must resolve in this bin's scope.

use aether_actor::{Addressable, ChildOf, One, Root, actor};

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

pub struct Parent;

impl Addressable for Parent {
    const NAMESPACE: &'static str = "test.struct_hosted_parent";
    type Resolver = One;
}

#[actor(instanced, root, child_of(Parent), rt_ok)]
pub struct Cap;

fn main() {
    fn root<T: Root>() {}
    fn child<T: ChildOf<Parent>>() {}
    root::<Cap>();
    child::<Cap>();
}
