//! Tiny scalar `f32` math for Aether: `Vec2`, `Vec3`, `Vec4`, `Mat4`, `Quat`.
//!
//! Designed for WASM guest components and native substrate alike —
//! `no_std`, no heap, no SIMD, no generics. Scalar code that LLVM +
//! wasm-opt can auto-vectorise when the deployment target enables
//! `simd128`, with an explicit SIMD feature to add later once a real
//! hot loop demands it (camera/transform math does not).
//!
//! # Conventions
//!
//! Two decisions are baked in at the type level. Changing them later
//! would ripple through every caller, so they are called out loudly:
//!
//! - **Column-major `Mat4`.** Stored as `[Vec4; 4]` where each `Vec4`
//!   is one column. This matches wgpu / GLSL / HLSL uniform upload
//!   layout, so a `Mat4` can be copied straight into a uniform buffer
//!   without transpose. `M * v` is "apply `M` to `v`" as in standard
//!   linear algebra.
//! - **YXZ Euler order.** `Quat::from_euler_yxz(yaw, pitch, roll)`
//!   applies yaw around `Y` (world up) first, then pitch around the
//!   rotated local `X` (right), then roll around the rotated local
//!   `Z` (forward). This is the natural order for an FPS / free-look
//!   camera. Other orders are not offered; add one only when a
//!   concrete use case forces it.
//!
//! World space is right-handed, `Y` up, `-Z` forward. Projection
//! matrices (`perspective_rh`, `orthographic_rh`) emit wgpu-style
//! clip space with depth in `[0, 1]` (not OpenGL's `[-1, 1]`), so
//! the output matrix uploads without any clip-space remap.

#![no_std]
#![forbid(unsafe_code)]

mod aabb;
mod mat;
mod quat;
#[cfg(test)]
mod test_helpers;
mod vec;

pub use aabb::Aabb;
pub use mat::Mat4;
pub use quat::Quat;
pub use vec::{Vec2, Vec3, Vec4};

// Re-export the standard math constants under our own crate root so
// downstream code can write `aether_math::PI` without rooting at
// `core::f32::consts`. The absolute path is intentional: importing
// `PI` / `TAU` at the crate root would shadow these re-exports and
// create a name cycle.
#[expect(
    clippy::absolute_paths,
    reason = "re-export would shadow itself if imported"
)]
pub const PI: f32 = core::f32::consts::PI;
#[expect(
    clippy::absolute_paths,
    reason = "re-export would shadow itself if imported"
)]
pub const TAU: f32 = core::f32::consts::TAU;
