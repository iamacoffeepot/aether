//! Issue 6279: a handler that spells its actor (`WasmCtx<'_, Self>`) receives
//! the macro-built typed ctx, and so does a `wire` hook spelling
//! `WireCtx<'_, '_, Self>`; a handler that spells no actor is typed by it too
//! (ADR-0231 §7, #6533) — all three compile on the wasm expansion.

use aether_actor::{WasmCtx, WireCtx, actor};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.pong")]
struct Pong {
    seq: u32,
}

struct TypedProbe;

#[actor]
impl aether_actor::WasmActor for TypedProbe {
    const NAMESPACE: &'static str = "typed_probe";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(TypedProbe)
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_, Self>) {
        let _ = ctx.sender();
    }

    #[handler::single]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_, Self>, ping: Ping) {
        let _ = (ctx.sender(), ping.seq);
    }

    #[handler::single]
    fn on_pong(&mut self, ctx: &mut WasmCtx<'_>, pong: Pong) {
        let _ = (ctx.sender(), pong.seq);
    }
}

fn main() {}
