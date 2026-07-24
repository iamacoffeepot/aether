use aether_actor::actor;

struct First;
struct Second;

#[actor(child_of(First, Second))]
pub struct Child;

fn main() {}
