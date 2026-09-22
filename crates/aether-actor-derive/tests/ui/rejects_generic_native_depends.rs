use aether_actor::actor;

struct Dependency;

struct Generic<T>(T);

#[actor(depends(Dependency))]
impl<T> NativeActor for Generic<T> {}

fn main() {}
