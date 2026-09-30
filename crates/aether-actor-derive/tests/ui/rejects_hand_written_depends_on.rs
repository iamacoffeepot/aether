//! ADR-0231 §10: a `DependsOn<R>` impl names `R`'s position in the actor's one
//! `Declared::Depends` list, which `#[actor(depends(..))]` writes from the same
//! list as the dependency entry the pre-`init` liveness check reads. An actor
//! that declares no dependency has an empty list, so a hand-written impl names
//! a position that holds nothing, is refused with `E0277`, and mints no
//! `ActorRef<R>` proof for an actor whose birth never checked that `R` was
//! `Live`.

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

    #[handler::tell]
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

impl aether_actor::DependsOn<Peer> for Hand {
    type Index = aether_actor::Here;
}

fn main() {}
