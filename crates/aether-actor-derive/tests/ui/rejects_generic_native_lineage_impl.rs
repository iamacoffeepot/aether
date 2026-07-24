use aether_actor::actor;

struct Generic<T>(T);

#[actor(root)]
impl<T> NativeActor for Generic<T> {}

fn main() {}
