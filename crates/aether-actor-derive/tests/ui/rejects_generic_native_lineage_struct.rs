use aether_actor::actor;

struct Parent;

#[actor(child_of(Parent), rt_ok)]
pub struct Generic<T>(T);

fn main() {}
