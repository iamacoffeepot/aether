//! A kindless nested type must not implement `Kind`.

#[derive(aether_data::Storage)]
struct Nested {
    x: u32,
}

fn main() {
    let _ = <Nested as aether_data::Kind>::ID;
}
