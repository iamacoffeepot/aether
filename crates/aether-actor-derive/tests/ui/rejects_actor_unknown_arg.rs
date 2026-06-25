//! ADR-0123: an unrecognised `#[actor]` argument is rejected at parse time,
//! before any struct/impl branch or disk read. `bogus = 1` is neither a known
//! key (`singleton` / `instanced` / `runtime_feature`) nor a bare module name
//! (the `= value` rules out the positional-ident branch).

use aether_actor::actor;

#[actor(bogus = 1)]
pub struct Cap;

fn main() {}
