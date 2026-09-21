use aether_actor::actor;

struct Target;

#[actor(depends(Target), depends(Target))]
pub struct Child;

fn main() {}
