//! ADR-0169: a `#[handler_set]` handler must carry a default body — the
//! shared behavior is what the set exists to carry. A required (bodyless)
//! handler method would put the kind on the set's dispatch chain while
//! leaving every adopter to supply the body, which is a plain trait method,
//! not a set member.

use aether_actor::handler_set;

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_set.bodyless")]
struct Ping {
    seq: u32,
}

#[handler_set]
trait Shared {
    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, ping: Ping);
}

fn main() {}
