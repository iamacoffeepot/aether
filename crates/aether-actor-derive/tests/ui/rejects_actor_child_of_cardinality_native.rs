use aether_actor::actor;

struct Parent;

struct ImplNative;

#[actor(child_of(Parent))]
impl NativeActor for ImplNative {}

#[actor(singleton, child_of(Parent), rt_ok)]
struct StructNative;

fn main() {}
