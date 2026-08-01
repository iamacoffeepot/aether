//! ADR-0169: a `#[handler_set]` trait and an actor adopting it through
//! `#[actor(handler_set(T))]` expand together and typecheck.
//!
//! Guards the whole delegation seam at once: the set's generated dispatch
//! method has to be callable as `<Self as Set>::…` from the adopter's
//! dispatch table, and the set's manifest const has to be usable in the
//! adopter's const-array length arithmetic. Either one going wrong is a
//! compile error here rather than a mystery at an adopter's call site.
//!
//! Also pins the override path: `Adopter` overrides `on_ping` and leaves
//! `on_pong` at the set's default, which is only well-formed if the set's
//! handlers are ordinary trait methods.

use aether_actor::{actor, handler_set};

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_set.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_set.pong")]
struct Pong {
    seq: u32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.handler_set.local")]
struct Local {
    seq: u32,
}

#[handler_set]
trait Shared {
    fn seen(&mut self) -> &mut u32;

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, ping: Ping) {
        *self.seen() += ping.seq;
    }

    #[handler::single]
    fn on_pong(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, pong: Pong) {
        *self.seen() += pong.seq;
    }
}

struct Adopter {
    seen: u32,
}

impl Shared for Adopter {
    fn seen(&mut self) -> &mut u32 {
        &mut self.seen
    }

    // Overriding a set member is an ordinary trait-method override.
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, ping: Ping) {
        self.seen = ping.seq;
    }
}

#[actor(handler_set(Shared))]
impl aether_actor::WasmActor for Adopter {
    const NAMESPACE: &'static str = "adopter";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(Adopter { seen: 0 })
    }

    #[handler::single]
    fn on_local(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, local: Local) {
        self.seen = local.seq;
    }
}

fn main() {}
