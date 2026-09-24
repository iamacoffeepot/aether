//! ADR-0230 §3: `DependsOn<R>` is an `unsafe trait` that only
//! `#[actor(depends(R))]` implements, because that expansion also records the
//! dependency entry the pre-`init` liveness check reads. A hand-written, safe
//! impl would mint `ActorRef<R>` proofs for an actor whose birth never checked
//! that `R` was `Live`, so it is refused with `E0200`.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.hand_written_depends_on.ping")]
struct Ping {
    seq: u32,
}

struct Peer;

#[actor]
impl WasmActor for Peer {
    const NAMESPACE: &'static str = "test.hand_written_depends_on.peer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Ping) {}
}

struct Hand;

#[actor]
impl WasmActor for Hand {
    const NAMESPACE: &'static str = "test.hand_written_depends_on.hand";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

impl aether_actor::DependsOn<Peer> for Hand {}

fn main() {}
