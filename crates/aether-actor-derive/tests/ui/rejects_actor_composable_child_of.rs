use aether_actor::actor;

struct ComposableThenChild;

#[actor(instanced, composable, child_of(FirstParent))]
impl aether_actor::WasmActor for ComposableThenChild {}

struct ChildThenComposable;

#[actor(instanced, child_of(SecondParent), composable)]
impl aether_actor::WasmActor for ChildThenComposable {}

fn main() {}
