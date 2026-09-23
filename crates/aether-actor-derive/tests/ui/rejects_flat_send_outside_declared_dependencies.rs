//! ADR-0232 §1–§2: the flat `ctx.send::<R>` verb compiles only for an `R` the
//! actor declares with `depends(R)`, and only for a kind `R` handles. The
//! declared, handled send compiles; a kind the peer has no handler for and a
//! send to an undeclared actor are each an `E0277`, so neither can warn-drop
//! at run time.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.flat_send.handled")]
struct Handled {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.flat_send.unhandled")]
struct Unhandled {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.flat_send.trigger")]
struct Trigger {
    seq: u32,
}

struct Peer;

#[actor]
impl WasmActor for Peer {
    const NAMESPACE: &'static str = "test.flat_send.peer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_handled(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Handled) {}
}

struct Stranger;

#[actor]
impl WasmActor for Stranger {
    const NAMESPACE: &'static str = "test.flat_send.stranger";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_handled(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Handled) {}
}

struct Sender;

#[actor(depends(Peer))]
impl WasmActor for Sender {
    const NAMESPACE: &'static str = "test.flat_send.sender";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_trigger(&mut self, ctx: &mut WasmCtx<'_, Self>, trigger: Trigger) {
        ctx.send::<Peer>(&Handled { seq: trigger.seq });
        ctx.send::<Peer>(&Unhandled { seq: trigger.seq });
        ctx.send::<Stranger>(&Handled { seq: trigger.seq });
    }
}

fn main() {}
