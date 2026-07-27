use aether_actor::actor;

#[actor(instanced, composable(argument))]
struct Argument;

#[actor(instanced, composable = true)]
struct Equals;

#[actor(instanced, composable, composable)]
struct Duplicate;

fn main() {}
