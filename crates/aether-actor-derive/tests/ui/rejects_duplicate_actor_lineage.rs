use aether_actor::actor;

struct Parent;

#[actor(child_of(Parent), child_of(Parent))]
pub struct Child;

fn main() {}
