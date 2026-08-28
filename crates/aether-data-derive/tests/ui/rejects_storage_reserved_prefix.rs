//! User-supplied names beginning with `__` are reserved.

#[derive(aether_data::Storage)]
#[kind(name = "test.reserved")]
struct Forbidden {
    __secret: u32,
}

fn main() {}
