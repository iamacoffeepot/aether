//! `#[storage(strict)]` is refused on a type without a kind name.

#[derive(aether_data::Storage)]
#[storage(strict)]
struct Nested {
    name: String,
}

fn main() {}
