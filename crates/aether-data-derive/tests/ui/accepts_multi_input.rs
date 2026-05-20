//! `transform_accepts_multi_input` — a multi-input (≤ 8) pure transform
//! compiles (ADR-0048 §1).

use aether_data::transform;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.foo")]
struct Foo {
    a: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.bar")]
struct Bar {
    b: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.baz")]
struct Baz {
    sum: u32,
}

/// Joins two inputs into a sum.
#[transform]
fn join(a: Foo, b: Bar) -> Baz {
    Baz {
        sum: a.a + b.b,
    }
}

fn main() {}
