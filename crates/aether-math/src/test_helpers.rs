//! Shared `f32` near-equality helpers for unit tests across `aether-math`.

#![cfg(test)]

use crate::{Vec3, Vec4};

/// Scalar `|a - b| < eps` predicate. The per-component primitive both
/// vector wrappers below feed each axis through.
fn near(a: f32, b: f32, eps: f32) -> bool {
    (a - b).abs() < eps
}

/// `true` iff every `Vec4` component pair is within `eps`.
pub fn approx_eq_vec4(a: Vec4, b: Vec4, eps: f32) -> bool {
    near(a.x, b.x, eps) && near(a.y, b.y, eps) && near(a.z, b.z, eps) && near(a.w, b.w, eps)
}

/// `Vec3` counterpart of [`approx_eq_vec4`].
pub fn approx_eq_vec3(a: Vec3, b: Vec3, eps: f32) -> bool {
    near(a.x, b.x, eps) && near(a.y, b.y, eps) && near(a.z, b.z, eps)
}
