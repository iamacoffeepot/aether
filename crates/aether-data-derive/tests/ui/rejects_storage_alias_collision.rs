//! A read alias whose hash matches a live sibling field is a collision.

#[derive(aether_data::Schema, aether_data::Storage)]
#[kind(name = "test.alias_collision")]
struct Record {
    id: u64,
    #[storage(was = "id")]
    ident: u64,
}

fn main() {}
