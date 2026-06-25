//! ADR-0123: a struct-hosted `#[actor]` whose runtime impl carries `#[handler]`s
//! but no `const NAMESPACE` is rejected — there is no name to lift into
//! `Addressable`.

use aether_actor::actor;

#[actor(singleton, rt_nonamespace)]
pub struct Cap;

fn main() {}
