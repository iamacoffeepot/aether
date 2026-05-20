//! `transform_accepts_pure_body` — a pure single-input transform
//! compiles (ADR-0048 §1).

use aether_data::transform;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.scalar")]
struct Scalar {
    value: u32,
}

/// Doubles the wrapped value. Pure — no host calls, no nondeterminism.
#[transform]
fn double(x: Scalar) -> Scalar {
    Scalar {
        value: x.value * 2,
    }
}

fn main() {}
