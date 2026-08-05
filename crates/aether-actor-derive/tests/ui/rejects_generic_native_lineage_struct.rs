use aether_actor::actor;

struct Parent;

// Keep placement valid so the generic-lineage diagnostic remains authoritative.
#[actor(instanced, child_of(Parent), rt_ok)]
pub struct Generic<T>(T);

fn main() {}
