//! ADR-0231 §2: a hand-written `CoveredBy` on a protocol collides with the
//! blanket impl, so a protocol cannot claim a target its rows do not cover.

use aether_actor::{ActorInitError, CoveredBy, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hand_written_covered_by_on_protocol.ping")]
struct Ping {
    seq: u32,
}

#[protocol]
trait Pinger {
    fn ping(mail: Ping);
}

struct Target;

#[actor]
impl WasmActor for Target {
    const NAMESPACE: &'static str = "test.hand_written_covered_by_on_protocol.target";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Ping) {}
}

impl CoveredBy<Target> for Pinger {}

fn main() {}
