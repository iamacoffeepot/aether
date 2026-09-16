//! `#[storage(validate)]` cannot share a type with `#[kind]`.

#[derive(aether_data::Storage)]
#[kind(name = "test.validate_kind")]
#[storage(validate)]
struct Named(String);

fn main() {}
