//! A struct-hosted actor harvests handler-set adoption from its sibling runtime
//! impl, so placing it on the struct attribute is rejected before that harvest.

use aether_actor::actor;

trait SharedHandlers {}

#[actor(singleton, handler_set(SharedHandlers))]
pub struct Cap;

fn main() {}
