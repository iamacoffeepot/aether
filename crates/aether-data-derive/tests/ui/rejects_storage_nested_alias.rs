//! Field aliases are refused on a kindless nested type.

#[derive(aether_data::Storage)]
struct Nested {
    #[storage(was = "old")]
    name: String,
}

fn main() {}
