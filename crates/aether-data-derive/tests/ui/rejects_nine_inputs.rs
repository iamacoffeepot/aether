//! `transform_rejects_nine_inputs` — a 9-parameter transform exceeds
//! the ADR-0048 §1 cap of 8 inputs and is rejected.

use aether_data::transform;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.scalar")]
struct Scalar {
    value: u32,
}

#[transform]
fn too_many(
    a: Scalar,
    b: Scalar,
    c: Scalar,
    d: Scalar,
    e: Scalar,
    f: Scalar,
    g: Scalar,
    h: Scalar,
    i: Scalar,
) -> Scalar {
    a
}

fn main() {}
