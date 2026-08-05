//! A native actor's second init is rejected at the later method rather than
//! silently replacing the first during expansion.

use aether_actor::actor;

struct Cap;

#[actor]
impl aether_substrate::actor::native::NativeActor for Cap {
    type Config = ();

    fn init() {}

    fn init() {}
}

fn main() {}
