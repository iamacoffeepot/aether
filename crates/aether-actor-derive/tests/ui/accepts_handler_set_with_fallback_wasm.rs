//! ADR-0169 §2: a wasm actor that adopts a handler set keeps its own
//! `#[fallback]` as the catch-all tail after the set misses.
//!
//! Pins the combination #6569 found broken: the set delegation moved the
//! inbound `Mail<'_>` into the set's dispatch method, and the `#[fallback]`
//! tail then used the same mail (E0382). The set now borrows the mail, so a
//! miss leaves it for the tail.

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
#[kind(name = "test.handler_set_fallback.ping")]
struct Ping {
    seq: u32,
}

#[handler_set]
trait Shared {
    fn seen(&mut self) -> &mut u32;

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, ping: Ping) {
        *self.seen() += ping.seq;
    }
}

struct Adopter {
    seen: u32,
}

impl Shared for Adopter {
    fn seen(&mut self) -> &mut u32 {
        &mut self.seen
    }
}

#[actor(handler_set(Shared))]
impl aether_actor::WasmActor for Adopter {
    const NAMESPACE: &'static str = "adopter";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(Adopter { seen: 0 })
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

fn main() {}
