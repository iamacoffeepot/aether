//! Shared `f32` near-equality helpers for unit tests across
//! `aether-math`. Pre-extraction, `mat::tests::approx_eq_vec4` and
//! `quat::tests::approx_eq_vec3` (and a couple of inline `(a - b).abs()
//! < EPS` ad-hoc checks elsewhere) repeated the same component-wise
//! diff body. Moved here so the comparison logic appears in one place.

#![cfg(test)]

use crate::{Vec3, Vec4};

/// Scalar `|a - b| < eps` predicate. The per-component primitive both
/// vector wrappers below feed each axis through.
fn near(a: f32, b: f32, eps: f32) -> bool {
    (a - b).abs() < eps
}

/// `true` iff every `Vec4` component pair is within `eps`.
pub fn approx_eq_vec4(a: Vec4, b: Vec4, eps: f32) -> bool {
    [(a.x, b.x), (a.y, b.y), (a.z, b.z), (a.w, b.w)]
        .iter()
        .all(|&(p, q)| near(p, q, eps))
}

/// `Vec3` counterpart of [`approx_eq_vec4`].
pub fn approx_eq_vec3(a: Vec3, b: Vec3, eps: f32) -> bool {
    [(a.x, b.x), (a.y, b.y), (a.z, b.z)]
        .iter()
        .all(|&(p, q)| near(p, q, eps))
}
