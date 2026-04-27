//! 3D point in 16:16 fixed-point coordinates — the integer grid the
//! BSP CSG core operates on.

use crate::csg::fixed::{FixedError, f32_to_fixed, fixed_to_f32};
use aether_math::Vec3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point3 {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl Point3 {
    pub fn from_f32(p: Vec3) -> Result<Self, FixedError> {
        Ok(Point3 {
            x: f32_to_fixed(p.x)?,
            y: f32_to_fixed(p.y)?,
            z: f32_to_fixed(p.z)?,
        })
    }

    pub fn to_f32(self) -> Vec3 {
        Vec3::new(
            fixed_to_f32(self.x),
            fixed_to_f32(self.y),
            fixed_to_f32(self.z),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::fixed::{MAX_INPUT_MAGNITUDE, SCALE};
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of<T: Hash>(v: &T) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    #[test]
    fn round_trip_identity_for_grid_aligned() {
        // Composes the fixed-layer guarantee at the Point3 level: any
        // grid-aligned input must come back bit-exactly. If this ever
        // fails it points at a regression in the per-component delegation
        // (e.g., axes silently swapped during refactor).
        let inputs = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, -2.5, 0.25),
            Vec3::new(MAX_INPUT_MAGNITUDE, -MAX_INPUT_MAGNITUDE, 0.0),
            Vec3::new(0.5, 0.75, -0.125),
        ];
        for input in inputs {
            let p = Point3::from_f32(input).unwrap();
            assert_eq!(p.to_f32(), input, "round-trip mismatch for {input:?}");
        }
    }

    #[test]
    fn axes_are_not_swapped() {
        // Catches a refactor that crosses x/y/z wires: e.g., setting
        // y from p.x. Distinct values per axis would slip past tests
        // that use Vec3::new(0, 0, 0)-style symmetric inputs.
        let p = Point3::from_f32(Vec3::new(1.0, 2.0, 3.0)).unwrap();
        let back = p.to_f32();
        assert_eq!(back, Vec3::new(1.0, 2.0, 3.0));
        // Also verify the underlying integer fields directly.
        assert_eq!(p.x, 1 << 16);
        assert_eq!(p.y, 2 << 16);
        assert_eq!(p.z, 3 << 16);
    }

    #[test]
    fn equal_grid_cell_hashes_equal() {
        // The welding invariant: two Point3s constructed from inputs
        // that snap to the same grid cell must compare equal AND hash
        // equal. If this breaks, weld silently produces duplicates and
        // we get phantom seams in the output mesh.
        let inputs = [
            (Vec3::new(0.5, 0.0, 0.0), Vec3::new(0.5, 0.0, 0.0)),
            (
                Vec3::new(0.123, -0.456, 0.789),
                Vec3::new(0.123, -0.456, 0.789),
            ),
            (Vec3::new(1.0, 1.0, 1.0), Vec3::new(1.0, 1.0, 1.0)),
        ];
        for (a_in, b_in) in inputs {
            let a = Point3::from_f32(a_in).unwrap();
            let b = Point3::from_f32(b_in).unwrap();
            assert_eq!(a, b, "grid-equivalent points should compare equal");
            assert_eq!(
                hash_of(&a),
                hash_of(&b),
                "grid-equivalent points must hash equal — weld depends on this"
            );
        }
    }

    #[test]
    fn negative_zero_collapses_to_zero_point() {
        // Pin the welding-relevant case from
        // `csg::fixed::negative_zero_collapses_to_zero` at the Point3
        // type — every component variant must collapse independently.
        let canonical = Point3::from_f32(Vec3::new(0.0, 0.0, 0.0)).unwrap();
        let variants = [
            Vec3::new(-0.0, 0.0, 0.0),
            Vec3::new(0.0, -0.0, 0.0),
            Vec3::new(0.0, 0.0, -0.0),
            Vec3::new(-0.0, -0.0, -0.0),
        ];
        for v in variants {
            let p = Point3::from_f32(v).unwrap();
            assert_eq!(p, canonical, "variant {v:?} did not collapse");
            assert_eq!(
                hash_of(&p),
                hash_of(&canonical),
                "variant {v:?} hashed differently from origin"
            );
        }
    }

    #[test]
    fn distinct_grid_cells_do_not_collide() {
        // Smallest representable step on each axis must produce a
        // distinct Point3. Documents that the grid resolution is
        // 1/SCALE = 1/65536 and points closer than that fold together.
        let one_ulp = 1.0 / SCALE as f32;
        let origin = Point3::from_f32(Vec3::new(0.0, 0.0, 0.0)).unwrap();
        let nudges = [
            Vec3::new(one_ulp, 0.0, 0.0),
            Vec3::new(0.0, one_ulp, 0.0),
            Vec3::new(0.0, 0.0, one_ulp),
        ];
        for n in nudges {
            let p = Point3::from_f32(n).unwrap();
            assert_ne!(p, origin, "{n:?} should sit in a different grid cell");
        }
    }

    #[test]
    fn first_bad_component_short_circuits() {
        // Pins the `?` order so diagnostic messages are deterministic.
        // If a future refactor switches to e.g. a `try_zip` over all
        // three components in parallel, the reported error could change
        // to "whichever happens to win the race" — which is exactly the
        // kind of silent-but-confusing diagnostic regression we want to
        // catch.
        let err = Point3::from_f32(Vec3::new(f32::NAN, f32::INFINITY, 999.0)).unwrap_err();
        match err {
            FixedError::NotFinite { value } => assert!(value.is_nan()),
            other => panic!("expected NotFinite for x=NaN, got {other:?}"),
        }
    }

    #[test]
    fn bad_x_is_reported() {
        let err = Point3::from_f32(Vec3::new(f32::NAN, 0.0, 0.0)).unwrap_err();
        assert!(matches!(err, FixedError::NotFinite { .. }));
    }

    #[test]
    fn bad_y_is_reported() {
        let err = Point3::from_f32(Vec3::new(0.0, 1e9, 0.0)).unwrap_err();
        assert!(matches!(err, FixedError::OutOfRange { value } if value == 1e9));
    }

    #[test]
    fn bad_z_is_reported() {
        let err = Point3::from_f32(Vec3::new(0.0, 0.0, f32::INFINITY)).unwrap_err();
        match err {
            FixedError::NotFinite { value } => assert_eq!(value, f32::INFINITY),
            other => panic!("expected NotFinite for z=Inf, got {other:?}"),
        }
    }

    #[test]
    fn ord_is_lexicographic_x_y_z() {
        // x dominates y, y dominates z. Pinning this so a future spatial-
        // sort refactor (e.g., morton order) doesn't silently break code
        // that relies on x-major scan order.
        let small_x = Point3::from_f32(Vec3::new(0.0, 999.0, 999.0)).unwrap_or_else(|_| {
            // 999 > MAX; fall back to in-range stand-in for the asymmetry.
            Point3::from_f32(Vec3::new(0.0, 100.0, 100.0)).unwrap()
        });
        let big_x = Point3::from_f32(Vec3::new(1.0, 0.0, 0.0)).unwrap();
        assert!(big_x > small_x, "x must dominate y/z in ordering");

        let small_y = Point3::from_f32(Vec3::new(5.0, 0.0, 999.0))
            .unwrap_or_else(|_| Point3::from_f32(Vec3::new(5.0, 0.0, 100.0)).unwrap());
        let big_y = Point3::from_f32(Vec3::new(5.0, 1.0, 0.0)).unwrap();
        assert!(big_y > small_y, "y must dominate z when x is equal");

        let small_z = Point3::from_f32(Vec3::new(5.0, 5.0, 0.0)).unwrap();
        let big_z = Point3::from_f32(Vec3::new(5.0, 5.0, 1.0)).unwrap();
        assert!(
            big_z > small_z,
            "z is the tiebreaker when x and y are equal"
        );
    }

    #[test]
    fn determinism() {
        // Same input → identical Point3 across N calls. Mirrors the
        // fixed-layer guarantee at this composition level.
        let input = Vec3::new(0.123, -0.456, 0.789);
        let first = Point3::from_f32(input).unwrap();
        for _ in 0..16 {
            assert_eq!(Point3::from_f32(input).unwrap(), first);
        }
    }
}
