use aether_actor::actor;

struct ImplNative;

#[actor(instanced, composable)]
impl NativeActor for ImplNative {}

#[actor(instanced, composable, rt_ok)]
struct StructNative;

fn main() {}
