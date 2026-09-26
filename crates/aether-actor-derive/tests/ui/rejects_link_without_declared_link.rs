//! ADR-0230 §2: `ctx.link::<R>` compiles only for an actor that declares
//! `links(R)`. The struct-hosted `Unit` reads the sibling `rt_ok.rs`.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::LoadName;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.unlinked.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.unlinked.pong")]
struct Pong {
    seq: u32,
}

#[actor(instanced, root, rt_ok)]
pub struct Unit;

struct Bootstrap;

#[actor]
impl WasmActor for Bootstrap {
    const NAMESPACE: &'static str = "test.unlinked.bootstrap";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {
        let key = LoadName::new("alpha").expect("a valid key");
        let _unit = ctx.link::<Unit>(&key);
    }
}

fn main() {}
