//! ADR-0123: a struct-hosted `#[actor]` whose runtime module file does not
//! exist on disk is a hard error — the sibling `rt_absent.rs` / `rt_absent/mod.rs`
//! cannot be read, so the identity can't be lifted.

use aether_actor::actor;

#[actor(singleton, rt_absent)]
pub struct Cap;

fn main() {}
