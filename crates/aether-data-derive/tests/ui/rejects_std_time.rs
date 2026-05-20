//! `transform_rejects_std_time` — a transform body that reads
//! `std::time::Instant::now()` is rejected by the deny-list scan as a
//! nondeterminism source (ADR-0048 §1).

use aether_data::transform;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.scalar")]
struct Scalar {
    value: u32,
}

#[transform]
fn bad(x: Scalar) -> Scalar {
    let _ = std::time::Instant::now();
    x
}

fn main() {}
