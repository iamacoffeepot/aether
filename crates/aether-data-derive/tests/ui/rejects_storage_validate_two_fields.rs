//! `#[storage(validate)]` is only valid on a single-field tuple struct.

#[derive(aether_data::Storage)]
#[storage(validate)]
struct Two {
    a: String,
    b: String,
}

fn main() {}
