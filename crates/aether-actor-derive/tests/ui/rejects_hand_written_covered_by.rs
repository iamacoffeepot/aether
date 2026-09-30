//! ADR-0231 §2: `CoveredBy` is sealed to its one blanket impl over `Protocol`,
//! so coverage cannot be claimed by hand. On a type that is not a protocol the
//! private seal is unsatisfied. The same impl on a protocol collides with the
//! blanket, which `rejects_hand_written_covered_by_on_protocol` pins: rustc
//! stops at that coherence error before it checks this one.

use aether_actor::{ActorInitError, CoveredBy, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hand_written_covered_by.ping")]
struct Ping {
    seq: u32,
}

struct NotAProtocol;

struct Target;

#[actor]
impl WasmActor for Target {
    const NAMESPACE: &'static str = "test.hand_written_covered_by.target";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Ping) {}
}

impl CoveredBy<Target> for NotAProtocol {}

fn main() {}
