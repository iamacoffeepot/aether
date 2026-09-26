//! ADR-0230 §2, ADR-0231 §3: `#[actor(links(..))]` emits `LinksTo<R>` on the
//! struct-hosted native form and on wasm, and the guest verbs compile for a
//! declared link: `ctx.link::<Unit>` for a root instance, `.narrow::<P>()` for
//! a protocol `Unit` covers, and `ctx.link_child::<Unit, Member>` beneath it.
//! Both typed paths are kind fields, with no bound on their parameter. The
//! struct-hosted `Unit` reads the sibling `rt_ok.rs`, whose `Ping` handler
//! answers `Pong`, so it covers `Pinging`.

use aether_actor::{
    ActorInitError, ActorPath, Addressable, ChildOf, LinksTo, Mail, Many, ProtocolPath, WasmActor, WasmCtx,
    WasmInitCtx, actor, protocol,
};
use aether_data::LoadName;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.links.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.links.pong")]
struct Pong {
    seq: u32,
}

#[protocol]
trait Pinging {
    fn ping(mail: Ping) -> Pong;
}

#[actor(instanced, root, links(Member), rt_ok)]
pub struct Unit;

pub struct Member;

impl Addressable for Member {
    const NAMESPACE: &'static str = "test.links.member";
    type Resolver = Many;
}

impl ChildOf<Unit> for Member {}

struct Bootstrap;

#[actor(links(Unit, Member))]
impl WasmActor for Bootstrap {
    const NAMESPACE: &'static str = "test.links.bootstrap";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {
        let key = LoadName::new("alpha").expect("a valid key");
        let unit: ActorPath<Unit> = ctx.link::<Unit>(&key);
        let _pinging: ProtocolPath<Pinging> = unit.narrow::<Pinging>();
        let _member: ActorPath<Member> = ctx.link_child::<Unit, Member>(&unit, &key).expect("under the caps");
    }
}

#[aether_data::kind(name = "test.links.paths")]
struct Paths {
    unit: ActorPath<Unit>,
    pinging: ProtocolPath<Pinging>,
}

fn main() {
    fn links<T: LinksTo<Member>>() {}
    links::<Unit>();
}
