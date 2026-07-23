//! Struct-hosted `#[actor]` module-path form: `#[actor(singleton,
//! nested::rt_nested)]` resolves the runtime module *relative to this file*
//! through the path segments — `nested/rt_nested.rs` — instead of a sibling
//! flat file. This is the headless-companion layout (`runtime::headless`)
//! in fixture form; the harvest, marker emission, and `include_bytes!`
//! rebuild edge must all compile exactly as the sibling-ident form does.

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
#[kind(name = "test.ping_struct_nested")]
struct Ping {
    seq: u32,
}

#[actor(singleton, nested::rt_nested)]
pub struct Cap;

fn main() {}
