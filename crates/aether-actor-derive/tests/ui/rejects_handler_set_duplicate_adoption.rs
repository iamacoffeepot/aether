//! ADR-0169 §4: an actor adopts at most one handler set. Two `handler_set`
//! arguments would make the dispatch order a chain rather than the two-step
//! local-then-set statement the ADR fixes.

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
#[kind(name = "test.handler_set.dup")]
struct Ping {
    seq: u32,
}

trait A {}
trait B {}

struct Adopter;

#[actor(handler_set(A), handler_set(B))]
impl aether_actor::WasmActor for Adopter {
    const NAMESPACE: &'static str = "adopter";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(Adopter)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
