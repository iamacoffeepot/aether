//! ADR-0231 §3: `ActorPath<R>::narrow::<P>()` compiles only for
//! `P: CoveredBy<R>`. The struct-hosted `Unit` reads the sibling `rt_ok.rs`,
//! whose one handler answers `Ping` with `Pong`, so it lacks `Other`'s `Poke`
//! row.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};
use aether_data::LoadName;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.uncovered.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.uncovered.pong")]
struct Pong {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.uncovered.poke")]
struct Poke {
    seq: u32,
}

#[protocol]
trait Other {
    fn ping(mail: Ping) -> Pong;
    fn poke(mail: Poke);
}

#[actor(instanced, root, rt_ok)]
pub struct Unit;

struct Bootstrap;

#[actor(links(Unit))]
impl WasmActor for Bootstrap {
    const NAMESPACE: &'static str = "test.uncovered.bootstrap";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {
        let key = LoadName::new("alpha").expect("a valid key");
        let _other = ctx.link::<Unit>(&key).narrow::<Other>();
    }
}

fn main() {}
