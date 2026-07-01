//! ADR-0123 gap 1: a struct-hosted `#[actor]` whose runtime module carries two
//! `#[handler]`-bearing `impl NativeActor` blocks (e.g. a platform-cfg'd dual
//! runtime impl) is rejected — the cfg-blind harvest cannot pick between them,
//! so it refuses rather than silently taking the first.

use aether_actor::actor;

#[actor(singleton, rt_ambiguous)]
pub struct Cap;

fn main() {}
