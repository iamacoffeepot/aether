// A type without #[actor] / #[reactor] companion metadata cannot be classified.

use aether_actor::export;

struct Plain;

export!(Plain, generators = [aether_bloomery_bundle::bundle]);

fn main() {}
