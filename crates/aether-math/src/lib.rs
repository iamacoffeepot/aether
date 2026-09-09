//! Tiny scalar `f32` math: `Vec2`, `Vec3`, `Vec4`, `Mat4`, `Quat`, `Aabb`,
//! `Rect2`, `Rigid`, and colors. `no_std`, no heap, no SIMD, no generics, so
//! wasm guest components and the native substrate share one set of types.
//!
//! # Conventions
//!
//! Two decisions are baked into the types, and changing either would ripple
//! through every caller:
//!
//! - **Column-major `Mat4`.** Stored as `[Vec4; 4]`, one `Vec4` per column.
//!   That matches wgpu / GLSL / HLSL uniform layout, so a `Mat4` copies
//!   straight into a uniform buffer with no transpose. `M * v` applies `M` to
//!   `v`, as in standard linear algebra.
//! - **YXZ Euler order.** `Quat::from_euler_yxz(yaw, pitch, roll)` applies yaw
//!   around `Y` (world up), then pitch around the rotated local `X` (right),
//!   then roll around the rotated local `Z` (forward): the natural order for
//!   an FPS or free-look camera. No other order is offered.
//!
//! World space is right-handed, `Y` up, `-Z` forward. `perspective_rh` and
//! `orthographic_rh` emit wgpu-style clip space with depth in `[0, 1]`, not
//! OpenGL's `[-1, 1]`, so the matrix uploads without a clip-space remap.

#![no_std]
#![forbid(unsafe_code)]

mod aabb;
mod color;
mod mat;
mod quat;
mod rect;
mod rigid;
#[cfg(test)]
mod test_helpers;
mod vec;

pub use aabb::{Aabb, Axis};
pub use color::{Hsl, Rgb, Rgba};
pub use mat::Mat4;
pub use quat::Quat;
pub use rect::Rect2;
pub use rigid::Rigid;
pub use vec::{Vec2, Vec3, Vec4};

// Re-export the standard math constants under our own crate root so
// downstream code can write `aether_math::PI` without rooting at
// `core::f32::consts`. The absolute path is intentional: importing
// `PI` / `TAU` at the crate root would shadow these re-exports and
// create a name cycle.
#[expect(clippy::absolute_paths, reason = "re-export would shadow itself if imported")]
pub const PI: f32 = core::f32::consts::PI;
#[expect(clippy::absolute_paths, reason = "re-export would shadow itself if imported")]
pub const TAU: f32 = core::f32::consts::TAU;
