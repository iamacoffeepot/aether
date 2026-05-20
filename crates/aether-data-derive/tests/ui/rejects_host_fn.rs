//! `transform_rejects_host_fn` — a transform body that calls a host fn
//! (`aether::send_mail_p32`) is rejected by the deny-list scan
//! (ADR-0048 §1).

use aether_data::transform;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.scalar")]
struct Scalar {
    value: u32,
}

#[transform]
fn bad(x: Scalar) -> Scalar {
    aether::send_mail_p32(0, 0, 0, 0);
    x
}

fn main() {}
