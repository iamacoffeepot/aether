//! `#[repr(C)]` on a storage type is a derive-time error.

#[repr(C)]
#[derive(aether_data::Schema, aether_data::Storage)]
#[kind(name = "test.storage_repr")]
struct Forbidden {
    x: u32,
}

fn main() {}
