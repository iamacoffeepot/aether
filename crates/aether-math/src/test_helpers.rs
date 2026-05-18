//! Shared `f32` near-equality helpers for unit tests across
//! `aether-math`. Pre-extraction, `mat::tests::approx_eq_vec4` and
//! `quat::tests::approx_eq_vec3` (and a couple of inline `(a - b).abs()
//! < EPS` ad-hoc checks elsewhere) repeated the same component-wise
//! diff body. Moved here so the comparison logic appears in one place.

#![cfg(test)]

use crate::{Vec3, Vec4};

/// Per-component absolute-difference check for a `Vec4`. Returns
/// `true` iff every component differs from its counterpart by less
/// than `eps`.
pub fn approx_eq_vec4(a: Vec4, b: Vec4, eps: f32) -> bool {
    (a.x - b.x).abs() < eps
        && (a.y - b.y).abs() < eps
        && (a.z - b.z).abs() < eps
        && (a.w - b.w).abs() < eps
}

/// `Vec3` counterpart of [`approx_eq_vec4`].
pub fn approx_eq_vec3(a: Vec3, b: Vec3, eps: f32) -> bool {
    (a.x - b.x).abs() < eps && (a.y - b.y).abs() < eps && (a.z - b.z).abs() < eps
}
