//! Flattened leaf hashes are checked pairwise; an alias that matches a
//! nested live leaf is a collision.

#[derive(aether_data::Storage)]
#[kind(name = "test.inner")]
struct Inner {
    x: u32,
}

#[derive(aether_data::Storage)]
#[kind(name = "test.leaf_collision")]
struct Record {
    inner: Inner,
    #[storage(was = "inner.x")]
    extra: u32,
}

fn main() {}
