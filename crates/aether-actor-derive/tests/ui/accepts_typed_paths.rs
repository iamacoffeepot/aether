//! ADR-0230 §2, ADR-0231 §3: typed paths come from type constructors, from a
//! downstream crate on derive-emitted markers. `ActorPath::<Unit>::instance`
//! writes a root instance's path, `.narrow::<P>()` a protocol `Unit` covers,
//! and `ActorPath::<Member>::child` a path beneath it. Both typed paths are
//! kind fields. The struct-hosted `Unit` reads the sibling `rt_ok.rs`, whose
//! `Ping` handler answers `Pong`, so it covers `Pinging`.

use aether_actor::{ActorPath, Addressable, ChildOf, Many, ProtocolPath, actor, protocol};
use aether_data::LoadName;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.typed_paths.ping")]
pub struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.typed_paths.pong")]
pub struct Pong {
    seq: u32,
}

#[protocol]
trait Pinging {
    fn ping(mail: Ping) -> Pong;
}

#[actor(instanced, root, rt_ok)]
pub struct Unit;

pub struct Member;

impl Addressable for Member {
    const NAMESPACE: &'static str = "test.typed_paths.member";
    type Resolver = Many;
}

impl ChildOf<Unit> for Member {}

#[aether_data::kind(name = "test.typed_paths.paths", no_serde)]
struct Paths {
    unit: ActorPath<Unit>,
    pinging: ProtocolPath<Pinging>,
}

fn main() {
    let key = LoadName::new("alpha").expect("a valid key");
    let unit = ActorPath::<Unit>::instance(&key);
    let _pinging: ProtocolPath<Pinging> = unit.narrow::<Pinging>();
    let _member: ActorPath<Member> = ActorPath::<Member>::child(&unit, &key).expect("under the caps");
}
