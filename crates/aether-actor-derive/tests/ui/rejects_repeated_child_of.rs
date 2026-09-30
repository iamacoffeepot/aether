//! ADR-0166 (issue 7210): an actor's parents are one `child_of(A, B)` list,
//! as its dependencies are one `depends(..)` list, so a second `child_of(..)`
//! in the same attribute is refused, and so is a parent named twice in it.

use aether_actor::actor;

struct First;
struct Second;

#[actor(child_of(First), child_of(Second))]
pub struct Repeated;

#[actor(child_of(First, First))]
pub struct Duplicated;

fn main() {}
