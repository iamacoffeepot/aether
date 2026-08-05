use aether_actor::actor;

struct DefaultRoot;

#[actor(root)]
impl aether_actor::WasmActor for DefaultRoot {}

struct InstancedRoot;

#[actor(instanced, root)]
impl aether_actor::WasmActor for InstancedRoot {}

fn main() {}
