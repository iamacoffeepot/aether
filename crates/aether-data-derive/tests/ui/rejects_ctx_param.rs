//! `transform_rejects_ctx_param` — a transform whose body names the
//! handler-context type `aether_actor::Ctx` is rejected by the
//! deny-list scan (ADR-0048 §1).

use aether_data::transform;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.scalar")]
struct Scalar {
    value: u32,
}

#[transform]
fn bad(x: Scalar) -> Scalar {
    let _ = aether_actor::Ctx::nothing();
    x
}

fn main() {}
