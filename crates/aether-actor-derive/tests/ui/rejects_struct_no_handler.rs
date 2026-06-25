//! ADR-0123: a struct-hosted `#[actor]` whose runtime module has no
//! `#[handler]`-bearing impl is rejected — there is nothing to lift.

use aether_actor::actor;

#[actor(singleton, rt_nohandler)]
pub struct Cap;

fn main() {}
