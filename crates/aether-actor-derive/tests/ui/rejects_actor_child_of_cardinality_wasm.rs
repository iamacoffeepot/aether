use aether_actor::actor;

struct Parent;

struct DefaultChild;

#[actor(child_of(Parent))]
impl aether_actor::WasmActor for DefaultChild {}

struct SingletonChild;

#[actor(singleton, child_of(Parent))]
impl aether_actor::WasmActor for SingletonChild {}

fn main() {}
