//! Shared fixtures for `aether-mesh` unit tests.
//!
//! `pt(x, y, z)` constructs a `Point3` from `f32` literals, encoding each
//! coordinate through `f32_to_fixed` and unwrapping. Inline-construction
//! convenience for tests; not part of the public API.

use crate::fixed::f32_to_fixed;
use crate::point::Point3;

pub(crate) fn pt(x: f32, y: f32, z: f32) -> Point3 {
    Point3 {
        x: f32_to_fixed(x).unwrap(),
        y: f32_to_fixed(y).unwrap(),
        z: f32_to_fixed(z).unwrap(),
    }
}
