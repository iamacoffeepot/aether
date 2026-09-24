//! ADR-0230 §3 (issue 6557): an actor's dependencies are one `depends(A, B)`
//! list, so a second `depends(...)` in the same attribute is refused.

use aether_actor::actor;

struct First;
struct Second;

#[actor(depends(First), depends(Second))]
pub struct Child;

fn main() {}
