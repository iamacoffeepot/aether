//! A set type is outside the Schema vocabulary — rejected at the derive
//! rather than compiling into serde.

use std::collections::HashSet;

#[derive(aether_data::Schema)]
struct Unique {
    values: HashSet<u32>,
}

fn main() {}
