use aether_actor::actor;

struct DefaultComposable;

#[actor(composable)]
impl aether_actor::WasmActor for DefaultComposable {}

struct SingletonComposable;

#[actor(singleton, composable)]
impl aether_actor::WasmActor for SingletonComposable {}

fn main() {}
